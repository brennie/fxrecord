// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use libfxrecord::{run, CommonOptions};
use libfxrecorder::config::Config;
use libfxrecorder::proto::RecorderProto;
use libfxrecorder::retry::delayed_exponential_retry;
use slog::{error, info, Logger};
use structopt::StructOpt;
use tokio::net::TcpStream;

#[derive(Debug, StructOpt)]
#[structopt(name = "fxrecorder", about = "Start FxRecorder")]
struct Options {
    /// The configuration file to use.
    #[structopt(long = "config", default_value = "fxrecord.toml")]
    config_path: PathBuf,

    /// The ID of a build task that will be used by the runner.
    task_id: String,
}

impl CommonOptions for Options {
    fn config_path(&self) -> &Path {
        &self.config_path
    }
}

fn main() {
    run::<Options, Config, _, _>(fxrecorder, "fxrecorder");
}

async fn fxrecorder(log: Logger, options: Options, config: Config) -> Result<(), Box<dyn Error>> {
    {
        let stream = TcpStream::connect(&config.host).await?;
        info!(log, "Connected"; "peer" => config.host);

        let mut proto = RecorderProto::new(log.clone(), stream);

        proto.handshake(true).await?;
    }

    {
        let reconnect = || {
            info!(log, "Attempting re-connection to runner...");
            TcpStream::connect(&config.host)
        };

        // This will attempt to reconnect for 0:30 + 1:00 + 2:00 + 4:00 = 7:30.
        let stream = delayed_exponential_retry(reconnect, Duration::from_secs(30), 4)
            .await
            .map_err(|e| {
                error!(
                    log,
                    "Could not connect to runner";
                    "last_error" => ?e.source().unwrap()
                );
                e
            })?;

        info!(log, "Re-connected"; "peer" => config.host);

        let mut proto = RecorderProto::new(log, stream);

        proto.handshake(false).await?;
        proto.download_build(&options.task_id).await?
    }

    Ok(())
}
