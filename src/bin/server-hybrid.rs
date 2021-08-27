use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::Poll;

use hyper::HeaderMap;
use hyper::{Body, Request, Response, body::HttpBody};
use tonic::async_trait;
use tonic_example::echo_server::{Echo, EchoServer};
use tonic_example::{EchoReply, EchoRequest};
use tower::Service;
use pin_project::pin_project;

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

    let axum_service = axum::Router::new().route("/", axum::handler::get(|| async { "Hello world!" }));

    let grpc_service = tonic::transport::Server::builder()
        .add_service(EchoServer::new(MyEcho))
        .into_service();

    let hybrid_make_service = hybrid(axum_service.into_make_service(), grpc_service);

    let server = hyper::Server::bind(&addr).serve(hybrid_make_service);

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}

fn hybrid<MakeWeb, Grpc>(make_web: MakeWeb, grpc: Grpc) -> HybridMakeService<MakeWeb, Grpc>
{
    HybridMakeService {
        make_web,
        grpc,
    }
}

struct HybridMakeService<MakeWeb, Grpc> {
    make_web: MakeWeb,
    grpc: Grpc,
}

impl<T, MakeWeb, Grpc> Service<T> for HybridMakeService<MakeWeb, Grpc>
where
    MakeWeb: Service<T>,
    Grpc: Clone,
{
    type Response = HybridService<MakeWeb::Response, Grpc>;
    type Error = MakeWeb::Error;
    type Future = HybridMakeServiceFuture<MakeWeb::Future, Grpc>;

    fn poll_ready(&mut self, cx: &mut std::task::Context) -> std::task::Poll<Result<(), Self::Error>> {
        self.make_web.poll_ready(cx)
    }

    fn call(&mut self, req: T) -> Self::Future {
        HybridMakeServiceFuture {
            web_future: self.make_web.call(req),
            grpc: Some(self.grpc.clone()),
        }
    }
}

#[pin_project]
struct HybridMakeServiceFuture<WebFuture, Grpc> {
    #[pin]
    web_future: WebFuture,
    grpc: Option<Grpc>,
}

impl<WebFuture, Web, WebError, Grpc> Future for HybridMakeServiceFuture<WebFuture, Grpc>
where
    WebFuture: Future<Output = Result<Web, WebError>>
{
    type Output = Result<HybridService<Web, Grpc>, WebError>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
        let this = self.project();
        match this.web_future.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(web)) => Poll::Ready(Ok(HybridService {
                web,
                grpc: this.grpc.take().expect("Cannot poll twice!"),
            }))
        }
    }
}

struct HybridService<Web, Grpc> {
    web: Web,
    grpc: Grpc,
}

impl<Web, Grpc, WebBody, GrpcBody> Service<Request<Body>> for HybridService<Web, Grpc>
where
    Web: Service<Request<Body>, Response = Response<WebBody>>,
    Grpc: Service<Request<Body>, Response = Response<GrpcBody>>,
    Web::Error: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    Grpc::Error: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
{
    type Response = Response<HybridBody<WebBody, GrpcBody>>;
    type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
    type Future = HybridFuture<Web::Future, Grpc::Future>;

    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
        match self.web.poll_ready(cx) {
            Poll::Ready(Ok(())) => match self.grpc.poll_ready(cx) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
                Poll::Pending => Poll::Pending,
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if req.headers().get("content-type").map(|x| x.as_bytes()) == Some(b"application/grpc") {
            HybridFuture::Right(self.grpc.call(req))
        } else {
            HybridFuture::Left(self.web.call(req))
        }
    }
}

enum HybridError<A, B> {
    Web(A),
    Grpc(B),
}

impl<A, B> std::fmt::Display for HybridError<A, B>
where
    A: std::fmt::Display,
    B: std::fmt::Display,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Web(a) => std::fmt::Display::fmt(a, f),
            Self::Grpc(b) => std::fmt::Display::fmt(b, f),
        }
    }
}

impl<A, B> std::fmt::Debug for HybridError<A, B>
where
    A: std::fmt::Debug,
    B: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Web(a) => std::fmt::Debug::fmt(a, f),
            Self::Grpc(b) => std::fmt::Debug::fmt(b, f),
        }
    }
}

impl<A: std::error::Error, B: std::error::Error> std::error::Error for HybridError<A, B> {}

enum HybridBody<A, B> {
    Web(A),
    Grpc(B),
}

impl<A, B> HttpBody for HybridBody<A, B>
where
    A: HttpBody + Send + Unpin,
    B: HttpBody<Data = A::Data> + Send + Unpin,
{
    type Data = A::Data;
    type Error = HybridError<A::Error, B::Error>;

    fn is_end_stream(&self) -> bool {
        match self {
            HybridBody::Web(b) => b.is_end_stream(),
            HybridBody::Grpc(b) => b.is_end_stream(),
        }
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match self.get_mut() {
            HybridBody::Web(b) => Pin::new(b).poll_data(cx).map_err(HybridError::Web),
            HybridBody::Grpc(b) => Pin::new(b).poll_data(cx).map_err(HybridError::Grpc),
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        match self.get_mut() {
            HybridBody::Web(b) => Pin::new(b).poll_trailers(cx).map_err(HybridError::Web),
            HybridBody::Grpc(b) => Pin::new(b).poll_trailers(cx).map_err(HybridError::Grpc),
        }
    }
}

#[pin_project(project = HybridFutureProj)]
enum HybridFuture<A, B> {
    Left(#[pin] A),
    Right(#[pin] B),
}

impl<A, B, BodyA, BodyB, ErrorA, ErrorB> Future for HybridFuture<A, B>
where
    A: Future<Output = Result<Response<BodyA>, ErrorA>>,
    B: Future<Output = Result<Response<BodyB>, ErrorB>>,
    ErrorA: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ErrorB: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
{
    type Output = Result<Response<HybridBody<BodyA, BodyB>>, Box<dyn std::error::Error + Send + Sync + 'static>>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
        match self.project() {
            HybridFutureProj::Left(a) => match a.poll(cx) {
                Poll::Ready(Ok(res)) => Poll::Ready(Ok(res.map(HybridBody::Web))),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
                Poll::Pending => Poll::Pending,
            },
            HybridFutureProj::Right(b) => match b.poll(cx) {
                Poll::Ready(Ok(res)) => Poll::Ready(Ok(res.map(HybridBody::Grpc))),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}
