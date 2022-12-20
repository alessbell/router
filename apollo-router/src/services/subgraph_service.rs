//! Tower fetcher for subgraphs.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;
use std::task::Poll;

use ::serde::Deserialize;
use futures::future::BoxFuture;
use global::get_text_map_propagator;
use http::header::ACCEPT;
use http::header::CONTENT_TYPE;
use http::header::{self};
use http::HeaderValue;
use hyper::Body;
use mime::APPLICATION_JSON;
use opentelemetry::global;
use schemars::JsonSchema;
use tower::util::BoxService;
use tower::BoxError;
use tower::Service;
use tower::ServiceExt;
use tracing::Instrument;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use super::layers::content_negociation::GRAPHQL_JSON_RESPONSE_HEADER_VALUE;
use super::subgraph_http;
use super::subgraph_http_service::SubgraphHTTPCreator;
use super::subgraph_http_service::SubgraphHTTPServiceFactory;
use super::Plugins;
use crate::error::FetchError;
use crate::graphql;
use crate::plugins::telemetry::LOGGING_DISPLAY_BODY;
use crate::plugins::telemetry::LOGGING_DISPLAY_HEADERS;

#[derive(PartialEq, Debug, Clone, Deserialize, JsonSchema, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Compression {
    /// gzip
    Gzip,
    /// deflate
    Deflate,
    /// brotli
    Br,
}

impl Display for Compression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Compression::Gzip => write!(f, "gzip"),
            Compression::Deflate => write!(f, "deflate"),
            Compression::Br => write!(f, "br"),
        }
    }
}

/// Client for interacting with subgraphs.
#[derive(Clone)]
pub(crate) struct SubgraphService {
    http_creator: SubgraphHTTPCreator, //Decompression<hyper::Client<HttpsConnector<HttpConnector>>>,
    service_name: Arc<String>,
}

impl SubgraphService {
    pub(crate) fn new(service_name: impl Into<String>, http_creator: SubgraphHTTPCreator) -> Self {
        Self {
            http_creator,
            service_name: Arc::new(service_name.into()),
        }
    }
}

impl tower::Service<crate::SubgraphRequest> for SubgraphService {
    type Response = crate::SubgraphResponse;
    type Error = BoxError;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: crate::SubgraphRequest) -> Self::Future {
        let crate::SubgraphRequest {
            subgraph_request,
            context,
            ..
        } = request;

        let mut client = self.http_creator.create(&self.service_name);
        let service_name = (*self.service_name).to_owned();

        Box::pin(async move {
            let (parts, body) = subgraph_request.into_parts();
            let body = serde_json::to_vec(&body).expect("JSON serialization should not fail");
            let mut request = http::request::Request::from_parts(parts, Body::from(body));

            let app_json: HeaderValue = HeaderValue::from_static(APPLICATION_JSON.essence_str());
            let app_graphql_json: HeaderValue =
                HeaderValue::from_static(GRAPHQL_JSON_RESPONSE_HEADER_VALUE);
            request.headers_mut().insert(CONTENT_TYPE, app_json.clone());
            request.headers_mut().insert(ACCEPT, app_json);
            request.headers_mut().append(ACCEPT, app_graphql_json);

            get_text_map_propagator(|propagator| {
                propagator.inject_context(
                    &Span::current().context(),
                    &mut opentelemetry_http::HeaderInjector(request.headers_mut()),
                );
            });

            let schema_uri = request.uri();
            let host = schema_uri.host().map(String::from).unwrap_or_default();
            let port = schema_uri.port_u16().unwrap_or_else(|| {
                let scheme = schema_uri.scheme_str();
                if scheme == Some("https") {
                    443
                } else if scheme == Some("http") {
                    80
                } else {
                    0
                }
            });
            let display_headers = context.contains_key(LOGGING_DISPLAY_HEADERS);
            let display_body = context.contains_key(LOGGING_DISPLAY_BODY);
            if display_headers {
                tracing::info!(http.request.headers = ?request.headers(), apollo.subgraph.name = %service_name, "Request headers to subgraph {service_name:?}");
            }
            if display_body {
                tracing::info!(http.request.body = ?request.body(), apollo.subgraph.name = %service_name, "Request body to subgraph {service_name:?}");
            }

            let path = schema_uri.path().to_string();

            let subgraph_http_request = subgraph_http::Request {
                subgraph_request: request,
                context: context.clone(),
            };

            let subgraph_http::Response { response, context } = client
                .call(subgraph_http_request)
                .instrument(tracing::info_span!("subgraph_request",
                    "otel.kind" = "CLIENT",
                    "net.peer.name" = &display(host),
                    "net.peer.port" = &display(port),
                    "http.route" = &display(path),
                    "net.transport" = "ip_tcp",
                    "apollo.subgraph.name" = %service_name
                ))
                .await
                .map_err(|err| {
                    tracing::error!(fetch_error = format!("{:?}", err).as_str());

                    FetchError::SubrequestHttpError {
                        service: service_name.clone(),
                        reason: err.to_string(),
                    }
                })?;

            // Keep our parts, we'll need them later
            let (parts, body) = response.into_parts();
            if display_headers {
                tracing::info!(
                    http.response.headers = ?parts.headers, apollo.subgraph.name = %service_name, "Response headers from subgraph {service_name:?}"
                );
            }
            if let Some(content_type) = parts.headers.get(header::CONTENT_TYPE) {
                if let Ok(content_type_str) = content_type.to_str() {
                    // Using .contains because sometimes we could have charset included (example: "application/json; charset=utf-8")
                    if !content_type_str.contains(APPLICATION_JSON.essence_str())
                        && !content_type_str.contains(GRAPHQL_JSON_RESPONSE_HEADER_VALUE)
                    {
                        return if !parts.status.is_success() {
                            Err(BoxError::from(FetchError::SubrequestHttpError {
                                service: service_name.clone(),
                                reason: format!(
                                    "{}: {}",
                                    parts.status.as_str(),
                                    parts.status.canonical_reason().unwrap_or("Unknown")
                                ),
                            }))
                        } else {
                            Err(BoxError::from(FetchError::SubrequestHttpError {
                                service: service_name.clone(),
                                reason: format!("subgraph didn't return JSON (expected content-type: {} or content-type: {GRAPHQL_JSON_RESPONSE_HEADER_VALUE}; found content-type: {content_type:?})", APPLICATION_JSON.essence_str()),
                            }))
                        };
                    }
                }
            }

            let body = hyper::body::to_bytes(body)
                .instrument(tracing::debug_span!("aggregate_response_data"))
                .await
                .map_err(|err| {
                    tracing::error!(fetch_error = format!("{:?}", err).as_str());

                    FetchError::SubrequestHttpError {
                        service: service_name.clone(),
                        reason: err.to_string(),
                    }
                })?;

            if display_body {
                tracing::info!(
                    http.response.body = %String::from_utf8_lossy(&body), apollo.subgraph.name = %service_name, "Raw response body from subgraph {service_name:?} received"
                );
            }

            let graphql: graphql::Response = tracing::debug_span!("parse_subgraph_response")
                .in_scope(|| {
                    graphql::Response::from_bytes(&service_name, body).map_err(|error| {
                        FetchError::SubrequestMalformedResponse {
                            service: service_name.clone(),
                            reason: error.to_string(),
                        }
                    })
                })?;

            let resp = http::Response::from_parts(parts, graphql);

            Ok(crate::SubgraphResponse::new_from_response(resp, context))
        })
    }
}

pub(crate) trait SubgraphServiceFactory: Clone + Send + Sync + 'static {
    type SubgraphService: Service<
            crate::SubgraphRequest,
            Response = crate::SubgraphResponse,
            Error = BoxError,
            Future = Self::Future,
        > + Send
        + 'static;
    type Future: Send + 'static;

    fn create(&self, name: &str) -> Option<Self::SubgraphService>;
}

#[derive(Clone)]
pub(crate) struct SubgraphCreator {
    pub(crate) http_creator: SubgraphHTTPCreator,
    pub(crate) services: Arc<HashMap<String, Arc<dyn MakeSubgraphService>>>,
    pub(crate) plugins: Arc<Plugins>,
}

impl SubgraphCreator {
    pub(crate) fn new(
        services: HashMap<String, Arc<dyn MakeSubgraphService>>,
        http_creator: SubgraphHTTPCreator,
        plugins: Arc<Plugins>,
    ) -> Self {
        SubgraphCreator {
            services: Arc::new(services),
            http_creator,
            plugins,
        }
    }
}

/// make new instances of the subgraph service
///
/// there can be multiple instances of that service executing at any given time
pub(crate) trait MakeSubgraphService: Send + Sync + 'static {
    fn make(&self) -> BoxService<crate::SubgraphRequest, crate::SubgraphResponse, BoxError>;
}

impl<S> MakeSubgraphService for S
where
    S: Service<crate::SubgraphRequest, Response = crate::SubgraphResponse, Error = BoxError>
        + Clone
        + Send
        + Sync
        + 'static,
    <S as Service<crate::SubgraphRequest>>::Future: Send,
{
    fn make(&self) -> BoxService<crate::SubgraphRequest, crate::SubgraphResponse, BoxError> {
        self.clone().boxed()
    }
}

impl SubgraphServiceFactory for SubgraphCreator {
    type SubgraphService = BoxService<crate::SubgraphRequest, crate::SubgraphResponse, BoxError>;
    type Future =
        <BoxService<crate::SubgraphRequest, crate::SubgraphResponse, BoxError> as Service<
            crate::SubgraphRequest,
        >>::Future;

    fn create(&self, name: &str) -> Option<Self::SubgraphService> {
        self.services.get(name).map(|_s| {
            let service = SubgraphService::new(name, self.http_creator.clone()).boxed();
            self.plugins
                .iter()
                .rev()
                .fold(service, |acc, (_, e)| e.subgraph_service(name, acc))
        })
    }
}

// #[cfg(test)]
// mod tests {
//     use std::convert::Infallible;
//     use std::net::SocketAddr;
//     use std::str::FromStr;

//     use axum::Server;
//     use http::header::HOST;
//     use http::StatusCode;
//     use http::Uri;
//     use hyper::service::make_service_fn;
//     use hyper::Body;
//     use serde_json_bytes::ByteString;
//     use serde_json_bytes::Value;
//     use tower::service_fn;
//     use tower::ServiceExt;

//     use super::*;
//     use crate::graphql::Error;
//     use crate::graphql::Request;
//     use crate::graphql::Response;
//     use crate::query_planner::fetch::OperationKind;
//     use crate::Context;
//     use crate::SubgraphHTTPRequest;

//     // starts a local server emulating a subgraph returning status code 400
//     async fn emulate_subgraph_bad_request(socket_addr: SocketAddr) {
//         async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
//             Ok(http::Response::builder()
//                 .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
//                 .status(StatusCode::BAD_REQUEST)
//                 .body(
//                     serde_json::to_string(&Response {
//                         errors: vec![Error::builder().message("This went wrong").build()],
//                         ..Response::default()
//                     })
//                     .expect("always valid")
//                     .into(),
//                 )
//                 .unwrap())
//         }

//         let make_svc = make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(handle)) });
//         let server = Server::bind(&socket_addr).serve(make_svc);
//         server.await.unwrap();
//     }

//     // starts a local server emulating a subgraph returning status code 401
//     async fn emulate_subgraph_unauthorized(socket_addr: SocketAddr) {
//         async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
//             Ok(http::Response::builder()
//                 .header(CONTENT_TYPE, "text/html")
//                 .status(StatusCode::UNAUTHORIZED)
//                 .body(r#""#.into())
//                 .unwrap())
//         }

//         let make_svc = make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(handle)) });
//         let server = Server::bind(&socket_addr).serve(make_svc);
//         server.await.unwrap();
//     }

//     // starts a local server emulating a subgraph returning bad response format
//     async fn emulate_subgraph_bad_response_format(socket_addr: SocketAddr) {
//         async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
//             Ok(http::Response::builder()
//                 .header(CONTENT_TYPE, "text/html")
//                 .status(StatusCode::OK)
//                 .body(r#"TEST"#.into())
//                 .unwrap())
//         }

//         let make_svc = make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(handle)) });
//         let server = Server::bind(&socket_addr).serve(make_svc);
//         server.await.unwrap();
//     }

//     // starts a local server emulating a subgraph returning compressed response
//     async fn emulate_subgraph_compressed_response(socket_addr: SocketAddr) {
//         async fn handle(request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
//             // Check the compression of the body
//             let mut encoder = GzipEncoder::new(Vec::new());
//             encoder
//                 .write_all(
//                     &serde_json::to_vec(&Request::builder().query("query".to_string()).build())
//                         .unwrap(),
//                 )
//                 .await
//                 .unwrap();
//             encoder.shutdown().await.unwrap();
//             let compressed_body = encoder.into_inner();
//             assert_eq!(
//                 compressed_body,
//                 hyper::body::to_bytes(request.into_body())
//                     .await
//                     .unwrap()
//                     .to_vec()
//             );

//             let original_body = Response {
//                 data: Some(Value::String(ByteString::from("test"))),
//                 ..Response::default()
//             };
//             let mut encoder = GzipEncoder::new(Vec::new());
//             encoder
//                 .write_all(&serde_json::to_vec(&original_body).unwrap())
//                 .await
//                 .unwrap();
//             encoder.shutdown().await.unwrap();
//             let compressed_body = encoder.into_inner();

//             Ok(http::Response::builder()
//                 .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
//                 .header(CONTENT_ENCODING, "gzip")
//                 .status(StatusCode::OK)
//                 .body(compressed_body.into())
//                 .unwrap())
//         }

//         let make_svc = make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(handle)) });
//         let server = Server::bind(&socket_addr).serve(make_svc);
//         server.await.unwrap();
//     }

//     #[tokio::test(flavor = "multi_thread")]
//     async fn test_bad_status_code_should_not_fail() {
//         let socket_addr = SocketAddr::from_str("127.0.0.1:2626").unwrap();
//         tokio::task::spawn(emulate_subgraph_bad_request(socket_addr));
//         let subgraph_service = SubgraphHTTPService::new("test");

//         let url = Uri::from_str(&format!("http://{}", socket_addr)).unwrap();
//         let response = subgraph_service
//             .oneshot(SubgraphHTTPRequest {
//                 supergraph_request: Arc::new(
//                     http::Request::builder()
//                         .header(HOST, "host")
//                         .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
//                         .body(Request::builder().query("query").build())
//                         .expect("expecting valid request"),
//                 ),
//                 subgraph_request: http::Request::builder()
//                     .header(HOST, "rhost")
//                     .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
//                     .uri(url)
//                     .body(Request::builder().query("query").build())
//                     .expect("expecting valid request"),
//                 operation_kind: OperationKind::Query,
//                 context: Context::new(),
//             })
//             .await
//             .unwrap();
//         assert_eq!(
//             response.response.body().errors[0].message,
//             "This went wrong"
//         );
//     }

//     #[tokio::test(flavor = "multi_thread")]
//     async fn test_bad_content_type() {
//         let socket_addr = SocketAddr::from_str("127.0.0.1:2525").unwrap();
//         tokio::task::spawn(emulate_subgraph_bad_response_format(socket_addr));
//         let subgraph_service = SubgraphHTTPService::new("test");

//         let url = Uri::from_str(&format!("http://{}", socket_addr)).unwrap();
//         let err = subgraph_service
//             .oneshot(SubgraphHTTPRequest {
//                 supergraph_request: Arc::new(
//                     http::Request::builder()
//                         .header(HOST, "host")
//                         .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
//                         .body(Request::builder().query("query").build())
//                         .expect("expecting valid request"),
//                 ),
//                 subgraph_request: http::Request::builder()
//                     .header(HOST, "rhost")
//                     .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
//                     .uri(url)
//                     .body(Request::builder().query("query").build())
//                     .expect("expecting valid request"),
//                 operation_kind: OperationKind::Query,
//                 context: Context::new(),
//             })
//             .await
//             .unwrap_err();
//         assert_eq!(
//             err.to_string(),
//             "HTTP fetch failed from 'test': subgraph didn't return JSON (expected content-type: application/json or content-type: application/graphql-response+json; found content-type: \"text/html\")"
//         );
//     }

//     #[tokio::test(flavor = "multi_thread")]
//     async fn test_compressed_request_response_body() {
//         let socket_addr = SocketAddr::from_str("127.0.0.1:2727").unwrap();
//         tokio::task::spawn(emulate_subgraph_compressed_response(socket_addr));
//         let subgraph_service = SubgraphHTTPService::new("test");

//         let url = Uri::from_str(&format!("http://{}", socket_addr)).unwrap();
//         let resp = subgraph_service
//             .oneshot(SubgraphHTTPRequest {
//                 supergraph_request: Arc::new(
//                     http::Request::builder()
//                         .header(HOST, "host")
//                         .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
//                         .body(Request::builder().query("query".to_string()).build())
//                         .expect("expecting valid request"),
//                 ),
//                 subgraph_request: http::Request::builder()
//                     .header(HOST, "rhost")
//                     .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
//                     .header(CONTENT_ENCODING, "gzip")
//                     .uri(url)
//                     .body(Request::builder().query("query".to_string()).build())
//                     .expect("expecting valid request"),
//                 operation_kind: OperationKind::Query,
//                 context: Context::new(),
//             })
//             .await
//             .unwrap();
//         // Test the right decompression of the body
//         let resp_from_subgraph = Response {
//             data: Some(Value::String(ByteString::from("test"))),
//             ..Response::default()
//         };

//         assert_eq!(resp.response.body(), &resp_from_subgraph);
//     }

//     #[tokio::test(flavor = "multi_thread")]
//     async fn test_unauthorized() {
//         let socket_addr = SocketAddr::from_str("127.0.0.1:2828").unwrap();
//         tokio::task::spawn(emulate_subgraph_unauthorized(socket_addr));
//         let subgraph_service = SubgraphHTTPService::new("test");

//         let url = Uri::from_str(&format!("http://{}", socket_addr)).unwrap();
//         let err = subgraph_service
//             .oneshot(SubgraphHTTPRequest {
//                 supergraph_request: Arc::new(
//                     http::Request::builder()
//                         .header(HOST, "host")
//                         .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
//                         .body(Request::builder().query("query").build())
//                         .expect("expecting valid request"),
//                 ),
//                 subgraph_request: http::Request::builder()
//                     .header(HOST, "rhost")
//                     .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
//                     .uri(url)
//                     .body(Request::builder().query("query").build())
//                     .expect("expecting valid request"),
//                 operation_kind: OperationKind::Query,
//                 context: Context::new(),
//             })
//             .await
//             .unwrap_err();
//         assert_eq!(
//             err.to_string(),
//             "HTTP fetch failed from 'test': 401: Unauthorized"
//         );
//     }
// }
