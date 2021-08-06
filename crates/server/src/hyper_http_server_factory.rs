use futures::channel::oneshot;
use futures::prelude::*;
use hyper::body::Bytes;
use hyper::header::{
    ACCEPT, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_ORIGIN, CONTENT_TYPE, HOST,
    LOCATION, ORIGIN,
};
use hyper::server::Server;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, StatusCode};
use parking_lot::RwLock;
use std::convert::Infallible;
use std::sync::Arc;

use configuration::Configuration;
use execution::{FetchError, GraphQLFetcher, GraphQLRequest};

use crate::http_server_factory::{HttpServerFactory, HttpServerHandle};
use crate::FederatedServerError;
use hyper::http::header::ACCESS_CONTROL_ALLOW_METHODS;
use hyper::http::HeaderValue;

/// A basic http server using hyper.
/// Uses streaming as primary method of response.
/// Redirects to studio for GET requests.
#[derive(Debug)]
pub(crate) struct HyperHttpServerFactory;

impl HyperHttpServerFactory {
    pub(crate) fn new() -> Self {
        HyperHttpServerFactory
    }
}

impl HttpServerFactory for HyperHttpServerFactory {
    fn create<F>(
        &self,
        graph: Arc<RwLock<F>>,
        configuration: Arc<RwLock<Configuration>>,
    ) -> HttpServerHandle
    where
        F: GraphQLFetcher + 'static,
    {
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let listen_address = configuration.read().server.listen;

        let server = Server::bind(&listen_address).serve(make_service_fn(move |_conn| {
            let graph = graph.to_owned();
            let configuration = configuration.to_owned();
            async move {
                Ok::<_, Infallible>(service_fn(move |req| {
                    serve_req(req, graph.clone(), configuration.clone())
                }))
            }
        }));
        let listen_address = server.local_addr().to_owned();
        let server_future = tokio::spawn(server.with_graceful_shutdown(async {
            shutdown_receiver.await.ok();
        }))
        .map(|result| match result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_err)) => Err(FederatedServerError::HttpServerLifecycleError),
            Err(_err) => Err(FederatedServerError::HttpServerLifecycleError),
        })
        .boxed();

        HttpServerHandle {
            shutdown_sender,
            server_future,
            listen_address,
        }
    }
}

async fn serve_req<F>(
    request: Request<Body>,
    graph: Arc<RwLock<F>>,
    configuration: Arc<RwLock<Configuration>>,
) -> Result<Response<Body>, hyper::Error>
where
    F: GraphQLFetcher,
{
    let mut response = Response::new(Body::empty());
    add_access_control_header(&configuration, &request, &mut response);
    match (request.method(), request.uri().path()) {
        (&Method::GET, "/") | (&Method::GET, "/graphql")
            if request
                .headers()
                .get_all(ACCEPT)
                .iter()
                .any(|header| match header.to_str() {
                    Ok(value) => value.contains("text/html"),
                    Err(_) => false,
                }) =>
        {
            handle_redirect_to_studio(request, &mut response)
        }
        (&Method::OPTIONS, "/") | (&Method::OPTIONS, "/graphql") => {
            handle_cors_preflight(&configuration, &mut response);
        }
        (_, "/") | (_, "/graphql") => handle_graphql_request(request, graph, &mut response).await,

        _ => {
            *response.status_mut() = StatusCode::NOT_FOUND;
            *response.body_mut() = Body::from("Not found");
        }
    };

    Ok(response)
}

fn add_access_control_header(
    configuration: &Arc<RwLock<Configuration>>,
    request: &Request<Body>,
    response: &mut Response<Body>,
) {
    let configuration = configuration.read();

    // If the host name matches one of the hosts specified in the config then return the hostname
    // in the cors header.
    if let Some(cors) = &configuration.server.cors {
        let headers = response.headers_mut();
        for cors_origin in &cors.origins {
            for header_origin in request.headers().get_all(ORIGIN) {
                if let Ok(orign) = header_origin.to_str() {
                    if orign == cors_origin {
                        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, header_origin.to_owned());
                    }
                }
            }
        }
    }
}

fn handle_cors_preflight(
    configuration: &Arc<RwLock<Configuration>>,
    response: &mut Response<Body>,
) {
    let configuration = configuration.read();
    if let Some(cors) = &configuration.server.cors {
        *response.status_mut() = StatusCode::NO_CONTENT;
        let headers = response.headers_mut();
        for method in &cors.methods {
            match &method.parse() {
                Ok(header_value) => {
                    headers.append(
                        ACCESS_CONTROL_ALLOW_METHODS,
                        HeaderValue::from(header_value),
                    );
                }
                Err(err) => {
                    log::error!(
                        "Failed to set {} header. {}",
                        ACCESS_CONTROL_ALLOW_METHODS,
                        err
                    );
                }
            }
        }
        headers.insert(
            ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from(CONTENT_TYPE),
        );
    }
}

async fn handle_graphql_request<F>(
    request: Request<Body>,
    graph: Arc<RwLock<F>>,
    response: &mut Response<Body>,
) where
    F: GraphQLFetcher,
{
    let (_header, body) = request.into_parts();
    // TODO Hardening. to_bytes does not reject huge requests.
    match hyper::body::to_bytes(body).await {
        Ok(bytes) => {
            let graphql_request = serde_json::from_slice::<GraphQLRequest>(&bytes);
            match graphql_request {
                Ok(graphql_request) => {
                    *response.body_mut() = Body::wrap_stream(
                        graph
                            .read()
                            .stream(graphql_request)
                            .map(|res| match res {
                                Ok(chunk) => match serde_json::to_string(&chunk) {
                                    Ok(bytes) => Ok(Bytes::from(bytes)),
                                    Err(_) => Err(FetchError::MalformedResponseError),
                                },
                                Err(err) => Err(err),
                            })
                            .boxed(),
                    )
                }
                Err(err) => {
                    *response.status_mut() = StatusCode::BAD_REQUEST;
                    *response.body_mut() = Body::from(format!("Request was malformed: {}", err));
                }
            }
        }
        Err(err) => {
            log::error!("Could not read request: {}", err);
            *response.status_mut() = StatusCode::BAD_REQUEST;
            *response.body_mut() = Body::from("Could not read request.");
        }
    }
}

fn handle_redirect_to_studio(request: Request<Body>, response: &mut Response<Body>) {
    *response.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    if let Some(header_value) = request
        .headers()
        .get(HOST)
        .and_then(|x| x.to_str().ok())
        .and_then(|x| {
            format!(
                "https://studio.apollographql.com/sandbox?endpoint=http://{}",
                x
            )
            .parse()
            .ok()
        })
    {
        response.headers_mut().insert(LOCATION, header_value);
    }

    *response.body_mut() = Body::from("");
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::error::Error;
    use std::net::SocketAddr;
    use std::str::FromStr;

    use futures::prelude::*;
    #[cfg(test)]
    use mockall::{mock, predicate::*};
    use reqwest::redirect::Policy;
    use reqwest::Client;
    use serde_json::json;

    use execution::{
        FetchError, GraphQLFetcher, GraphQLPrimaryResponse, GraphQLRequest, GraphQLResponse,
        GraphQLResponseStream,
    };

    use super::*;
    use configuration::Cors;

    macro_rules! assert_header {
        ($response:expr, $header:expr, $expected:expr $(, $msg:expr)?) => {
            assert_eq!(
                $response
                    .headers()
                    .get_all($header)
                    .iter()
                    .map(|v|v.to_str().unwrap().to_string())
                    .collect::<Vec<_>>(),
                $expected
                $(, $msg)*
            );
        };
    }

    mock! {
        #[derive(Debug)]
        MyGraphQLFetcher{}
        impl GraphQLFetcher for MyGraphQLFetcher {   // specification of the trait to mock
            fn stream(&self, request: GraphQLRequest) -> GraphQLResponseStream;
        }
    }

    fn init(listen_address: &str) -> (Arc<RwLock<MockMyGraphQLFetcher>>, HttpServerHandle, Client) {
        let _ = env_logger::builder().is_test(true).try_init();
        let fetcher = MockMyGraphQLFetcher::new();
        let server_factory = HyperHttpServerFactory::new();
        let fetcher = Arc::new(RwLock::new(fetcher));
        let server = server_factory.create(
            fetcher.to_owned(),
            Arc::new(RwLock::new(
                Configuration::builder()
                    .server(
                        configuration::Server::builder()
                            .listen(SocketAddr::from_str(listen_address).unwrap())
                            .cors(Some(
                                Cors::builder().origins(vec!["studio".to_string()]).build(),
                            ))
                            .build(),
                    )
                    .subgraphs(HashMap::new())
                    .build(),
            )),
        );
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .unwrap();
        (fetcher, server, client)
    }

    #[tokio::test]
    async fn redirect_to_studio() -> Result<(), FederatedServerError> {
        let (_fetcher, server, client) = init("127.0.0.1:0");

        for url in vec![
            format!("http://{}/", server.listen_address),
            format!("http://{}/graphql", server.listen_address),
        ] {
            let response = client
                .get(url)
                .header(ACCEPT, "text/html")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
            assert_header!(
                &response,
                LOCATION,
                vec![format!(
                    "https://studio.apollographql.com/sandbox?endpoint=http://{}",
                    server.listen_address
                )
                .to_string()],
                "Incorrect redirect url"
            );
        }

        server.shutdown().await
    }

    #[tokio::test]
    async fn malformed_request() -> Result<(), FederatedServerError> {
        let (_fetcher, server, client) = init("127.0.0.1:0");

        let response = client
            .post(format!("http://{}/graphql", server.listen_address))
            .body("Garbage")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        server.shutdown().await
    }

    #[tokio::test]
    async fn response() -> Result<(), FederatedServerError> {
        let expected_response = GraphQLPrimaryResponse {
            data: json!(
            {
              "response": "yay",
            })
            .as_object()
            .cloned()
            .unwrap(),
            has_next: None,
            errors: None,
            extensions: None,
        };
        let example_response = expected_response.clone();
        let (fetcher, server, client) = init("127.0.0.1:0");
        {
            fetcher
                .write()
                .expect_stream()
                .times(1)
                .return_once(move |_| {
                    futures::stream::iter(vec![Ok(GraphQLResponse::Primary(example_response))])
                        .boxed()
                });
        }
        let response = client
            .post(format!("http://{}/graphql", server.listen_address))
            .body(
                json!(
                {
                  "query": "query",
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .expect("unexpected response");

        assert_eq!(
            response.json::<GraphQLPrimaryResponse>().await.unwrap(),
            expected_response,
        );

        server.shutdown().await
    }

    #[tokio::test]
    async fn response_failure() -> Result<(), FederatedServerError> {
        let (fetcher, server, client) = init("127.0.0.1:0");
        {
            fetcher.write().expect_stream().times(1).return_once(|_| {
                futures::stream::iter(vec![Err(FetchError::ServiceError {
                    service: "Mock service".to_string(),
                    reason: "Mock error".to_string(),
                })])
                .boxed()
            });
        }
        let response = client
            .post(format!("http://{}/graphql", server.listen_address))
            .body(
                json!(
                {
                  "query": "query",
                })
                .to_string(),
            )
            .send()
            .await
            .err()
            .unwrap();

        // Why are we even testing this?
        // Basically for chunked encoding the only option to send errors back to the client are
        // via a trailer header, but even then, we can't send back an alternate error code.
        // Our only real option is to make sure that this code path doesn't happen and use graphql errors.
        // However, we don't want to bring down the server, so we don't panic.
        assert_eq!(
            format!("{:?}", response.source().unwrap()),
            "hyper::Error(IncompleteMessage)"
        );
        server.shutdown().await
    }

    #[tokio::test]
    async fn cors_preflight() -> Result<(), FederatedServerError> {
        let (_fetcher, server, client) = init("127.0.0.1:0");

        for url in vec![
            format!("http://{}/", server.listen_address),
            format!("http://{}/graphql", server.listen_address),
        ] {
            let response = client
                .request(Method::OPTIONS, &url)
                .header(ACCEPT, "text/html")
                .header(ORIGIN, "studio")
                .send()
                .await
                .unwrap();

            assert_header!(
                &response,
                ACCESS_CONTROL_ALLOW_ORIGIN,
                vec!["studio"],
                "Incorrect access control allow origin header"
            );
            assert_header!(
                &response,
                ACCESS_CONTROL_ALLOW_HEADERS,
                vec!["content-type"],
                "Incorrect access control allow header header"
            );
            assert_header!(
                &response,
                ACCESS_CONTROL_ALLOW_METHODS,
                vec!["GET", "POST", "OPTIONS"],
                "Incorrect access control allow methods header"
            );
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }

        server.shutdown().await
    }
}
