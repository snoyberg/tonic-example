use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::Poll;

use axum::body::BoxBody;
use futures::ready;
use hyper::service::make_service_fn;
use hyper::{Body, Request, Response};
use pin_project::pin_project;
use tonic::async_trait;
use tonic_example::echo_server::{Echo, EchoServer};
use tonic_example::{EchoReply, EchoRequest};
use tower::{Service, ServiceExt};

struct MyEcho;

#[async_trait]
impl Echo for MyEcho {
    async fn echo(
        &self,
        request: tonic::Request<EchoRequest>,
    ) -> Result<tonic::Response<EchoReply>, tonic::Status> {
        Ok(tonic::Response::new(EchoReply {
            message: format!("Echoing back: {}", request.get_ref().message),
        }))
    }
}

#[tokio::main]
async fn main() {
    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));

    let axum_make_service = axum::Router::new()
        .route("/", axum::handler::get(|| async { "Hello world!" }))
        // Needed to unify errors types from Infallible to tonic's Box<dyn std::error::Error + Send + Sync>
        .map_err(|i| match i {});

    let grpc_service = tonic::transport::Server::builder()
        .add_service(EchoServer::new(MyEcho))
        .into_service()
        // Needed to unify body type into axum's BoxBody
        .map_response(|response| {
            let (parts, body) = response.into_parts();
            Response::from_parts(parts, axum::body::box_body(body))
        });

    let hybrid_service = HybridService {
        web: axum_make_service,
        grpc: grpc_service,
    };

    let server = hyper::Server::bind(&addr).serve(make_service_fn(move |_conn| {
        let hybrid_service = hybrid_service.clone();
        async { Ok::<_, axum::Error>(hybrid_service) }
    }));

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}

#[derive(Clone)]
struct HybridService<Web, Grpc> {
    web: Web,
    grpc: Grpc,
}

impl<Web, Grpc, Error> Service<Request<Body>> for HybridService<Web, Grpc>
where
    Web: Service<Request<Body>, Response = Response<BoxBody>, Error = Error>,
    Grpc: Service<Request<Body>, Response = Response<BoxBody>, Error = Error>,
{
    type Response = Response<BoxBody>;
    type Error = Error;
    type Future = HybridFuture<Web::Future, Grpc::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Poll::Ready(if let Err(err) = ready!(self.web.poll_ready(cx)) {
            Err(err)
        } else {
            ready!(self.web.poll_ready(cx))
        })
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if req.headers().get("content-type").map(|x| x.as_bytes()) == Some(b"application/grpc") {
            HybridFuture::Grpc(self.grpc.call(req))
        } else {
            HybridFuture::Web(self.web.call(req))
        }
    }
}

#[pin_project(project = HybridFutureProj)]
enum HybridFuture<WebFuture, GrpcFuture> {
    Web(#[pin] WebFuture),
    Grpc(#[pin] GrpcFuture),
}

impl<WebFuture, GrpcFuture, Output> Future for HybridFuture<WebFuture, GrpcFuture>
where
    WebFuture: Future<Output = Output>,
    GrpcFuture: Future<Output = Output>,
{
    type Output = Output;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
        match self.project() {
            HybridFutureProj::Web(a) => a.poll(cx),
            HybridFutureProj::Grpc(b) => b.poll(cx),
        }
    }
}
