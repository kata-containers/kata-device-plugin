// SPDX-License-Identifier: Apache-2.0
// Copyright (c) NVIDIA CORPORATION

//! Publish the fabric view to kata-device-plugin, in one call.
//!
//! The plugin keeps the last view it was given, until the next call or until
//! it hands the fabric to a new holder, so the command that changes
//! partitions publishes the result and exits; nothing stays connected.  The
//! plugin hands the one container it gives the fabric to an endpoint and a
//! token through its environment ([`kata_fabric_api::ENDPOINT_ENV`],
//! [`kata_fabric_api::TOKEN_ENV`]).

use std::error::Error;
use std::time::Duration;

use kata_fabric_api::grpc::{with_token, FabricViewServiceClient};
use kata_fabric_api::v1::FabricView;
use tonic::transport::{Channel, Endpoint};
use tonic::Request;

/// Where to publish and how to authenticate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// `http://host:port`.
    pub endpoint: String,
    /// Absent when `KATA_FABRIC_VIEW_TOKEN` is unset or empty; the plugin
    /// then refuses the call.
    pub token: Option<String>,
}

impl Target {
    /// From the values the plugin injects ([`kata_fabric_api::ENDPOINT_ENV`],
    /// [`kata_fabric_api::TOKEN_ENV`])
    /// or a flag; `None` without an endpoint, and an empty token is no token.
    pub fn from_vars(endpoint: Option<String>, token: Option<String>) -> Option<Self> {
        let endpoint = endpoint.filter(|endpoint| !endpoint.is_empty())?;
        Some(Self {
            endpoint,
            token: token.filter(|token| !token.is_empty()),
        })
    }
}

/// One Publish call.  Fails for a token that is not valid gRPC metadata,
/// which no plugin issues, for an endpoint that cannot be reached, and for
/// a plugin that refuses the token or cannot keep the view.
pub async fn publish(target: &Target, view: FabricView) -> Result<(), Box<dyn Error>> {
    let channel = connect(&target.endpoint).await?;
    let mut request = Request::new(view);
    if let Some(token) = &target.token {
        request = with_token(request, token)?;
    }
    FabricViewServiceClient::new(channel)
        .publish(request)
        .await?;
    Ok(())
}

/// The connect timeout bounds an endpoint that drops packets, which the OS
/// would otherwise let hang for minutes; the request timeout bounds a plugin
/// that accepts the connection and does not answer, so the command reports
/// the publish as failed instead of waiting.
async fn connect(endpoint: &str) -> Result<Channel, tonic::transport::Error> {
    const TIMEOUT: Duration = Duration::from_secs(10);
    Endpoint::from_shared(endpoint.to_owned())?
        .connect_timeout(TIMEOUT)
        .timeout(TIMEOUT)
        .connect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use kata_fabric_api::grpc::{require_token, FabricViewService, FabricViewServiceServer};
    use kata_fabric_api::v1::{self, PublishResponse};
    use tokio_stream::wrappers::TcpListenerStream;
    use tokio_util::sync::CancellationToken;
    use tonic::{Code, Response, Status};

    /// A plugin stand-in: records every view.
    #[derive(Default)]
    struct Recorder {
        views: Mutex<Vec<FabricView>>,
    }

    /// The service handle tonic owns; the test keeps the Arc to read back.
    struct Service(Arc<Recorder>);

    #[tonic::async_trait]
    impl FabricViewService for Service {
        async fn publish(
            &self,
            request: Request<FabricView>,
        ) -> Result<Response<PublishResponse>, Status> {
            self.0.views.lock().unwrap().push(request.into_inner());
            Ok(Response::new(PublishResponse {}))
        }
    }

    struct Plugin {
        recorder: Arc<Recorder>,
        endpoint: String,
        stop: CancellationToken,
    }

    async fn plugin(token: &str) -> Plugin {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let recorder = Arc::new(Recorder::default());
        let stop = CancellationToken::new();
        let server = FabricViewServiceServer::with_interceptor(
            Service(recorder.clone()),
            require_token(token),
        );
        let shutdown = stop.clone();
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(server)
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                    shutdown.cancelled().await
                }),
        );
        Plugin {
            recorder,
            endpoint: format!("http://127.0.0.1:{port}"),
            stop,
        }
    }

    fn view(unit: &str, active: &[u32]) -> FabricView {
        FabricView {
            partitions: Some(v1::PartitionList {
                version: 1,
                max_num_partitions: 15,
                partition_info: active
                    .iter()
                    .map(|&id| v1::Partition {
                        partition_id: id,
                        is_active: true,
                        gpu_info: vec![v1::PartitionGpu::default(); 2],
                    })
                    .collect(),
            }),
            unsupported: None,
            unit: unit.to_owned(),
        }
    }

    fn target(plugin: &Plugin, token: Option<&str>) -> Target {
        Target {
            endpoint: plugin.endpoint.clone(),
            token: token.map(str::to_owned),
        }
    }

    fn code(result: Result<(), Box<dyn Error>>) -> Code {
        result
            .unwrap_err()
            .downcast_ref::<Status>()
            .expect("a gRPC status")
            .code()
    }

    #[test]
    fn target_from_vars() {
        assert_eq!(Target::from_vars(None, None), None);
        assert_eq!(
            Target::from_vars(Some(String::new()), Some("t".into())),
            None
        );
        assert_eq!(
            Target::from_vars(Some("http://10.0.0.1:9877".into()), Some(String::new())),
            Some(Target {
                endpoint: "http://10.0.0.1:9877".into(),
                token: None
            })
        );
        assert_eq!(
            Target::from_vars(Some("http://10.0.0.1:9000".into()), Some("t".into()))
                .unwrap()
                .token,
            Some("t".into())
        );
    }

    #[tokio::test]
    async fn each_call_hands_the_plugin_one_view() {
        let plugin = plugin("t").await;
        publish(&target(&plugin, Some("t")), view("", &[1]))
            .await
            .unwrap();
        publish(&target(&plugin, Some("t")), view("gpu", &[2]))
            .await
            .unwrap();
        assert_eq!(
            *plugin.recorder.views.lock().unwrap(),
            [view("", &[1]), view("gpu", &[2])]
        );
        plugin.stop.cancel();
    }

    #[tokio::test]
    async fn a_wrong_or_missing_token_is_refused() {
        let plugin = plugin("t").await;
        assert_eq!(
            code(publish(&target(&plugin, Some("wrong")), view("", &[1])).await),
            Code::Unauthenticated
        );
        assert_eq!(
            code(publish(&target(&plugin, None), view("", &[1])).await),
            Code::Unauthenticated
        );
        assert!(plugin.recorder.views.lock().unwrap().is_empty());
        plugin.stop.cancel();
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_an_error() {
        // A listener bound and dropped: the port refuses the connection.
        let closed = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let target = Target {
            endpoint: format!("http://127.0.0.1:{port}"),
            token: None,
        };
        assert!(publish(&target, view("", &[])).await.is_err());
    }
}
