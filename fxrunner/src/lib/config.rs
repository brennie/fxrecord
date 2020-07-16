// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

/// The configuration for FxRunner.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// The address and port to listen on.
    pub host: SocketAddr,

    /// The directory to store request state in.
    pub requests_dir: PathBuf,
}
