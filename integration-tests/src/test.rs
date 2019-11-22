// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::future::Future;

use assert_matches::assert_matches;
use chrono::prelude::*;
use futures::join;
use fxrecorder::proto::RecorderProto;
use fxrunner::proto::{RunnerProto, RunnerProtoError};
use fxrunner::shutdown::Shutdown;
use fxrunner::taskcluster::{
    Artifact, ArtifactsResponse, Taskcluster, TaskclusterError, BUILD_ARTIFACT_NAME,
};
use libfxrecord::error::ErrorMessage;
use libfxrecord::net::*;
use slog::Logger;
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use url::Url;

#[derive(Default)]
pub struct TestShutdown {
    error: Option<String>,
}

impl TestShutdown {
    pub fn with_error(s: &str) -> Self {
        TestShutdown {
            error: Some(s.into()),
        }
    }
}

impl Shutdown for TestShutdown {
    type Error = ErrorMessage<String>;

    fn initiate_restart(&self, _reason: &str) -> Result<(), Self::Error> {
        match self.error {
            Some(ref e) => Err(ErrorMessage(e.into())),
            None => Ok(()),
        }
    }
}

/// Generate a logger for testing.
///
/// The generated logger discards all messages.
fn test_logger() -> Logger {
    Logger::root(slog::Discard, slog::o! {})
}

/// Generate a Taskcluster instance that points at mockito.
fn test_tc() -> Taskcluster {
    Taskcluster::with_queue_url(
        Url::parse(&mockito::server_url())
            .unwrap()
            .join("/api/queue/v1/")
            .unwrap(),
    )
}

/// Run a test with both the recorder and runner protocols.
async fn run_proto_test<T, U>(
    listener: &mut TcpListener,
    shutdown: TestShutdown,
    runner_fn: impl FnOnce(RunnerProto<TestShutdown>) -> T,
    recorder_fn: impl FnOnce(RecorderProto) -> U,
) where
    T: Future<Output = ()>,
    U: Future<Output = ()>,
{
    let addr = listener.local_addr().unwrap();

    let runner = async {
        let (stream, _) = listener.accept().await.unwrap();
        let proto = RunnerProto::new(test_logger(), stream, shutdown, test_tc());

        runner_fn(proto).await;
    };

    let recorder = async {
        let stream = TcpStream::connect(&addr).await.unwrap();
        let proto = RecorderProto::new(test_logger(), stream);

        recorder_fn(proto).await;
    };

    join!(runner, recorder);
}

#[tokio::test]
async fn test_handshake() {
    let mut listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

    // Test runner dropping connection before receiving handshake.
    run_proto_test(
        &mut listener,
        TestShutdown::default(),
        |_| async move {},
        |mut recorder| {
            async move {
                // It is non-deterministic which error we will get.
                match recorder.handshake(false).await.unwrap_err() {
                    ProtoError::Io(..) => {}
                    ProtoError::EndOfStream => {}
                    e => panic!("unexpected error: {:?}", e),
                }
            }
        },
    )
    .await;

    // Test recorder dropping connection before handshaking.
    run_proto_test(
        &mut listener,
        TestShutdown::default(),
        |mut runner| {
            async move {
                assert_matches!(
                    runner.handshake_reply().await.unwrap_err(),
                    RunnerProtoError::Proto(ProtoError::EndOfStream)
                );
            }
        },
        |_| async move {},
    )
    .await;

    // Test runner dropping connection before end of handshake.
    run_proto_test(
        &mut listener,
        TestShutdown::default(),
        |runner| {
            async move {
                runner.into_inner().recv::<Handshake>().await.unwrap();
            }
        },
        |mut recorder| {
            async move {
                assert_matches!(
                    recorder.handshake(true).await.unwrap_err(),
                    ProtoError::EndOfStream
                );
            }
        },
    )
    .await;

    // Test handshake protocol.
    run_proto_test(
        &mut listener,
        TestShutdown::default(),
        |mut runner| {
            async move {
                assert!(runner.handshake_reply().await.unwrap());
            }
        },
        |mut recorder| {
            async move {
                recorder.handshake(true).await.unwrap();
            }
        },
    )
    .await;

    // Test handshake protocol with false.
    run_proto_test(
        &mut listener,
        TestShutdown::default(),
        |mut runner| {
            async move {
                assert!(!runner.handshake_reply().await.unwrap());
            }
        },
        |mut recorder| {
            async move {
                recorder.handshake(false).await.unwrap();
            }
        },
    )
    .await;

    // Test handshake protocol with failed shutdown.
    run_proto_test(
        &mut listener,
        TestShutdown::with_error("could not shutdown"),
        |mut runner| {
            async move {
                assert_matches!(runner.handshake_reply().await.unwrap_err(),
                    RunnerProtoError::Shutdown(e) => {
                        assert_eq!(e.to_string(), "could not shutdown");
                    }
                );
            }
        },
        |mut recorder| {
            use fxrecorder::proto::ProtoError;
            async move {
                assert_matches!(
                    recorder.handshake(true).await.unwrap_err(),
                    ProtoError::Foreign(e) => {
                        assert_eq!(e.to_string(), "could not shutdown");
                    }
                );
            }
        },
    )
    .await;
}

#[tokio::test]
async fn test_download_build() {
    let mut listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

    {
        let download_dir = TempDir::new().unwrap();
        let list_rsp = mockito::mock("GET", "/api/queue/v1/task/foo/artifacts")
            .with_body(
                serde_json::to_string(&ArtifactsResponse {
                    artifacts: vec![Artifact {
                        name: BUILD_ARTIFACT_NAME.into(),
                        expires: Utc::now()
                            .checked_add_signed(chrono::Duration::seconds(3600))
                            .unwrap(),
                    }],
                })
                .unwrap(),
            )
            .create();

        let artifact_rsp = mockito::mock(
            "GET",
            "/api/queue/v1/task/foo/artifacts/public/build/target.zip",
        )
        .with_body("foo")
        .create();

        run_proto_test(
            &mut listener,
            TestShutdown::default(),
            |mut runner| {
                async move {
                    runner
                        .download_build_reply(download_dir.path())
                        .await
                        .unwrap();
                }
            },
            |mut recorder| {
                async move {
                    recorder.download_build("foo").await.unwrap();
                }
            },
        )
        .await;

        list_rsp.assert();
        artifact_rsp.assert();
    }

    {
        let download_dir = TempDir::new().unwrap();
        let list_rsp = mockito::mock("GET", "/api/queue/v1/task/foo/artifacts")
            .with_body(serde_json::to_string(&ArtifactsResponse { artifacts: vec![] }).unwrap())
            .create();

        run_proto_test(
            &mut listener,
            TestShutdown::default(),
            |mut runner| {
                async move {
                    assert_matches!(
                        runner
                            .download_build_reply(download_dir.path())
                            .await
                            .unwrap_err(),
                        RunnerProtoError::Taskcluster(TaskclusterError::NotFound)
                    );
                }
            },
            |mut recorder| {
                async move {
                    assert_matches!(
                        recorder.download_build("foo").await.unwrap_err(),
                        ProtoError::Foreign(ErrorMessage(e)) => {
                            assert_eq!(e, TaskclusterError::NotFound.to_string());
                        }
                    );
                }
            },
        )
        .await;

        list_rsp.assert();
    }

    {
        let download_dir = TempDir::new().unwrap();
        let expiry = Utc::now()
            .checked_sub_signed(chrono::Duration::days(1))
            .unwrap();
        let list_rsp = mockito::mock("GET", "/api/queue/v1/task/foo/artifacts")
            .with_body(
                serde_json::to_string(&ArtifactsResponse {
                    artifacts: vec![Artifact {
                        name: BUILD_ARTIFACT_NAME.into(),
                        expires: expiry,
                    }],
                })
                .unwrap(),
            )
            .create();

        run_proto_test(
            &mut listener,
            TestShutdown::default(),
            |mut runner| {
                async move {
                    assert_matches!(
                        runner.download_build_reply(download_dir.path()).await.unwrap_err(),
                        RunnerProtoError::Taskcluster(TaskclusterError::Expired(e)) => {
                            assert_eq!(e, expiry);
                        }
                    );
                }
            },
            |mut recorder| {
                async move {
                    assert_matches!(
                        recorder.download_build("foo").await.unwrap_err(),
                        ProtoError::Foreign(ErrorMessage(e)) => {
                            assert_eq!(e, TaskclusterError::Expired(expiry).to_string());
                        }
                    );
                }
            },
        )
        .await;

        list_rsp.assert();
    }
}
