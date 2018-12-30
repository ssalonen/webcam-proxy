#![warn(unused_extern_crates)]
#![feature(duration_float)]
use std::time::{Duration, Instant};

use failure::Fail;
#[macro_use]
extern crate lazy_static;

use futures::future::{err, ok};
use futures::prelude::*;
use futures::sync::mpsc::{channel, Receiver, Sender};
use futures::try_ready;
use hyper::rt::{Future, Stream};
use hyper::{Body, Chunk, Request, Response, StatusCode, Uri};
use hyper_tls::HttpsConnector;
use image::jpeg::JPEGEncoder;
use image::Rgba;
use imageproc::drawing::{draw_filled_rect_mut, draw_text_mut};
use imageproc::rect::Rect;
use qutex::QrwLock;
use qutex::Qutex;
use rusttype::{Font, Scale};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::SocketAddr;
use std::thread::{self, JoinHandle};

use tokio::prelude::future::*;
use tokio::prelude::*;
use tokio::timer::Interval;

use rand::Rng;

static DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5);
static IMAGE_STALE_THRESHOLD: Duration = Duration::from_secs(20);
static CLIENT_STALE_THRESHOLD: Duration = Duration::from_secs(300);
static WIDTH: u32 = 1280;
static HEIGHT: u32 = 960;
lazy_static! {
    static ref FONT: Font<'static> = {
        let font_data: &[u8] = include_bytes!("../fonts/DejaVuSansMono.ttf");
        let font: Font<'static> = Font::from_bytes(font_data).expect("Font error");
        font
    };
    static ref WHITE_RGBA: Rgba<u8> = Rgba([255u8, 255u8, 255u8, 255u8]);
    static ref BLACK_RGBA: Rgba<u8> = Rgba([0u8, 0u8, 0u8, 255u8]);
}

type Clients = Vec<QrwLock<Client>>;
type HttpClient = hyper::Client<HttpsConnector<hyper::client::connect::HttpConnector>>;

pub struct Server {
    clients: Qutex<Clients>,
    download_url: Uri,
    image_data: QrwLock<ImageData>,
    auth: BTreeMap<String, String>,
}

pub struct ImageData {
    last_successful_image_raw: Vec<u8>,
    last_successful_image: Vec<u8>,
    last_success: Option<Instant>,
    last_error: Option<Instant>,
}

#[derive(Debug, Fail)]
enum DownloadError {
    #[fail(display = "generic download error")]
    Generic {},
    #[fail(display = "nonsuccesfull http status: {}", status)]
    NonSuccessfullStatus { status: u16 },
    #[fail(display = "download timeout")]
    Timeout {},
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ServerError();

impl std::error::Error for ServerError {
    fn description(&self) -> &str {
        "Generic error"
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Generic error occurred")
    }
}

fn append_mjpeg_image_single_client(image: Vec<u8>, client: &QrwLock<Client>) {
    hyper::rt::spawn(
        client
            .clone()
            .read()
            .map_err(|_| ())
            .and_then(move |client| {
                let client = &(*client);
                let sender = &client.channel.0;
                sender.clone().send(image).map(|_| ()).map_err(|_| ())
            })
            .map_err(|_| ()),
    );
}

impl Server {
    /// Create a new SSE push-server.
    pub fn new(download_url: Uri, auth: BTreeMap<String, String>) -> Server {
        use image::RgbImage;
        let image_buffer = RgbImage::new(WIDTH, HEIGHT);
        let mut image_jpeg_buffer = vec![];
        let mut encoder = JPEGEncoder::new(&mut image_jpeg_buffer);
        encoder
            .encode(&image_buffer, WIDTH, HEIGHT, image::RGB(8))
            .expect("Could not create blank start image!");
        Server {
            clients: Qutex::new(vec![]),
            download_url: download_url,
            auth: auth,
            image_data: QrwLock::new(ImageData {
                last_successful_image_raw: image_jpeg_buffer.clone(),
                last_successful_image: image_jpeg_buffer,
                last_success: Default::default(),
                last_error: Default::default(),
            }),
        }
    }

    fn remove_stale_clients(
        &'static self,
    ) -> Box<dyn Future<Item = (), Error = ()> + Send + 'static> {
        println!("Remove stale clients loop starting");
        Box::new(
            self.clients
                .clone()
                .lock()
                .map_err(|_| panic!("clients lock tainted. Bug?"))
                .and_then(|clients| {
                    let read_locks: Vec<_> = (*clients)
                        .iter()
                        .map(|client| client.clone().read())
                        .collect();
                    ok(read_locks)
                })
                .and_then(move |read_locks| {
                    loop_fn(read_locks, move |read_locks| {
                        if read_locks.is_empty() {
                            Either::A(ok(Loop::Break(())))
                        } else {
                            Either::B(
                                select_all(read_locks)
                                    .map_err(|_| ())
                                    .and_then(move |(handle, client_index, rest)| {
                                        let client = handle;
                                        let error_time = match (
                                            client.first_error,
                                            client.last_sent,
                                            client.created,
                                        ) {
                                            (Some(first_error), None, _) => first_error,
                                            (None, Some(last_sent), _) => last_sent,
                                            (Some(first_error), Some(last_sent), _) => {
                                                if first_error > last_sent {
                                                    first_error
                                                } else {
                                                    last_sent
                                                }
                                            }
                                            (None, None, created) => created,
                                        };
                                        if error_time.elapsed() > CLIENT_STALE_THRESHOLD {
                                            return Either::A(
                                                self.clients
                                                    .clone()
                                                    .lock()
                                                    .map_err(|_| {
                                                        panic!("clients lock tainted. Bug?")
                                                    })
                                                    .and_then(move |mut clients| {
                                                        println!(
                                                            "Removing stale client {}",
                                                            (*client).id
                                                        );
                                                        (*clients).remove(client_index);
                                                        ok(Loop::Continue(rest))
                                                    }),
                                            );
                                        } else {
                                            println!("client {} not stale", (*client).id);
                                        }
                                        Either::B(ok(Loop::Continue(rest)))
                                    })
                                    .map_err(|_| ()),
                            )
                        }
                    })
                    .map(|_| ())
                }),
        )
    }

    fn check_image_stale_async(&self) -> impl Future<Item = (), Error = ()> {
        self.image_data
            .clone()
            .read()
            .map_err(|_| panic!("image_data lock tainted. Bug?"))
            .and_then(|image_data| {
                let stale = match image_data.last_success {
                    Some(last_success) => last_success.elapsed() > IMAGE_STALE_THRESHOLD,
                    _ => true,
                };
                println!("Image stale: {:?}", stale);
                if stale {
                    ok(())
                } else {
                    err(())
                }
            })
            .join(
                self.image_data
                    .clone()
                    .write()
                    .map_err(|_| panic!("image_data lock tainted. Bug?")),
            )
            .and_then(|(_, mut image_data)| {
                let image_bits = image_data.last_successful_image_raw.clone();
                if let Ok(mut image) =
                    image::load_from_memory_with_format(&image_bits, image::ImageFormat::JPEG)
                {
                    draw_filled_rect_mut(
                        &mut image,
                        Rect::at(500, 500).of_size(100, 100),
                        *BLACK_RGBA,
                    );
                    draw_text_mut(
                        &mut image,
                        *WHITE_RGBA,
                        500,
                        500,
                        Scale::uniform(24.0),
                        &FONT,
                        "STALE!",
                    );
                    let last_successful_image = &mut (*image_data).last_successful_image;
                    last_successful_image.truncate(0);
                    let mut encoder = JPEGEncoder::new(&mut image_data.last_successful_image);
                    encoder
                        .encode(&image.to_rgb(), WIDTH, HEIGHT, image::RGB(8))
                        .expect("Could create stale image (jpeg encode)!");
                    ok(())
                } else {
                    err(())
                }
                // println!("Image stale finished");
            })
            .map_err(|_| ())
    }

    fn download_picture_async(
        &'static self,
        http_client: &HttpClient,
    ) -> impl Future<Item = (), Error = DownloadError> {
        http_client
            .get(self.download_url.clone())
            .map_err(|_| DownloadError::Generic {})
            .and_then(|response| {
                let (parts, body) = response.into_parts();
                match parts.status {
                    StatusCode::OK => {
                        println!("Download OK");
                        Ok(body)
                    }
                    status => {
                        println!("Status error");
                        Err(DownloadError::NonSuccessfullStatus {
                            status: status.as_u16(),
                        })
                    }
                }
            })
            .and_then(|body: hyper::Body| {
                body.fold(Vec::new(), |mut acc, chunk| {
                    acc.extend_from_slice(&*chunk);
                    futures::future::ok::<_, hyper::Error>(acc)
                })
                .map_err(|_| panic!("Unexpected error. Bug?"))
            })
            .timeout(DOWNLOAD_TIMEOUT)
            .map_err(|_| DownloadError::Timeout {})
            .and_then(move |body_data| {
                // let mut image_data = try_ready!(self.image_data.write().poll().or(Err(())));
                self.image_data
                    .clone()
                    .write()
                    .map_err(|_| panic!("image_data lock tainted. Bug?"))
                    .and_then(|mut image_data| {
                        image_data.last_successful_image_raw = body_data.clone();
                        image_data.last_successful_image = body_data;
                        image_data.last_success = Some(Instant::now());
                        Ok(())
                    })
            })
            .or_else(move |_| {
                self.image_data
                    .clone()
                    .write()
                    .map_err(|_| panic!("image_data lock tainted. Bug?"))
                    .and_then(|mut image_data| {
                        image_data.last_error = Some(Instant::now());
                        Ok(())
                    })
            })
            .then(move |_| {
                poll_fn(move || {
                    self.update_all_clients_async()
                        .or(Err(DownloadError::Generic {}))
                })
            })
            .into_future()
    }

    fn add_client(&self, sender: hyper::body::Sender) {
        let client = QrwLock::new(Client::new(sender));
        let client2 = client.clone();
        let client3 = client.clone();
        let image_data = self.image_data.clone();
        println!("Client added");
        hyper::rt::spawn(
            self.clients
                .clone()
                .lock()
                .map_err(|_| panic!("clients lock tainted. Bug?"))
                .and_then(|mut clients| {
                    (*clients).push(client);
                    ok(())
                })
                .map_err(|_| ())
                .and_then(|_| ImageWriterFuture::new(client2))
                .map_err(|_| ())
                .and_then(move |_| {
                    image_data
                        .read()
                        .map_err(|_| panic!("image_data lock tainted. Bug?"))
                })
                .map_err(|_| ())
                .and_then(move |image| {
                    append_mjpeg_image_single_client(
                        (*image).last_successful_image.clone(),
                        &client3,
                    );
                    ok(())
                })
                .map_err(|_| ()),
        );
    }

    fn update_all_clients_async(&self) -> Poll<(), ()> {
        let image_data = try_ready!(self.image_data.clone().read().poll().or(Err(())));
        self.append_mjpeg_image_all_clients_async(image_data.last_successful_image.clone())
    }

    fn append_mjpeg_image_all_clients_async(&self, image: Vec<u8>) -> Poll<(), ()> {
        let mut clients = try_ready!(self.clients.clone().lock().poll().or(Err(())));
        for client in clients.iter_mut() {
            append_mjpeg_image_single_client(image.clone(), &client);
        }
        Ok(Async::Ready(()))
    }

    /// Append image to MJPEG stream, for a single client

    pub fn spawn(&'static self, listen: SocketAddr) -> JoinHandle<()> {
        use hyper::service::service_fn;

        let sse_handler = move |req: Request<Body>| self.serve(&req);

        let http_server = hyper::Server::bind(&listen)
            .serve(move || service_fn(sse_handler))
            .map_err(|e| panic!("Push server failed: {}", e));

        let maintenance = Interval::new(Instant::now(), CLIENT_STALE_THRESHOLD)
            .for_each(move |_| {
                self.remove_stale_clients()
                    .inspect(|_| println!("loop finished"))
                    .or_else(|_| ok(()))
            })
            .map_err(|e| panic!("Push maintenance failed: {}", e));

        let stale_monitor = Interval::new(Instant::now(), Duration::from_secs(5))
            .for_each(move |_| {
                self.check_image_stale_async()
                    .then(|_| ok(()))
                    .then(move |_res: Result<(), ()>| {
                        poll_fn(move || self.update_all_clients_async())
                    })
                    .then(|_| ok(()))
            })
            .map_err(|e| panic!("Push maintenance failed: {}", e));

        let https = HttpsConnector::new(16).expect("TLS initialization failed");
        let http_client = hyper::Client::builder().build::<_, hyper::Body>(https);

        let download_new = Interval::new(Instant::now(), Duration::from_secs(1))
            .map_err(|e| panic!("Push download_new failed: {}", e))
            .for_each(move |_| self.download_picture_async(&http_client).then(|_| ok(())));

        thread::spawn(move || {
            hyper::rt::run(
                http_server
                    .join(maintenance)
                    .join(stale_monitor)
                    .join(download_new)
                    .map(|_| ()),
            );
        })
    }

    pub fn serve(
        &self,
        request: &Request<Body>,
    ) -> impl Future<Item = Response<Body>, Error = ServerError> {
        println!("headers: {:?}", request.headers());
        // Extract channel from uri path (last segment)
        if let Some(query) = request.uri().query() {
            let parsed_args = url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect::<HashMap<String, String>>();
            let user = parsed_args.get("usr");
            let password = parsed_args.get("pwd");
            let auth_ok = match (user, password) {
                (Some(user), Some(password)) => self.auth.get(user) == Some(password),
                _ => false,
            };
            if !auth_ok {
                return Either::A(ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Body::from("Unauthorized"))
                    .expect("Could not create response")));
            }
        // println!("parsedArgs {:?}", parsed_args);
        } else {
            return Either::A(ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Invalid query string"))
                .expect("Could not create response")));
        }
        println!("path {:?}", request.uri().path());
        match request.uri().path() {
            "/snapshot" => Either::B(
                self.image_data
                    .clone()
                    .read()
                    .and_then(|image| {
                        let image_bytes = (*image).last_successful_image.clone();
                        ok(Response::builder()
                            .header("Cache-Control", "no-cache")
                            .header("Connection", "close")
                            .header("Content-Type", "image/jpeg")
                            .header("Pragma", "no-cache")
                            .body(Body::from(image_bytes))
                            .expect("Could not create response"))
                    })
                    .map_err(|_| ServerError()),
            ),
            "/stream" => {
                let (sender, body) = Body::channel();
                self.add_client(sender);
                Either::A(ok(Response::builder()
                    .header("Cache-Control", "no-cache")
                    .header("Connection", "close")
                    .header(
                        "Content-Type",
                        "multipart/x-mixed-replace; boundary=BOUNDARY",
                    )
                    .header("Pragma", "no-cache")
                    .body(body)
                    .expect("Could not create response")))
            }
            _ => Either::A(ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Invalid path"))
                .expect("Could not create response"))),
        }
    }
}

struct Client {
    tx: hyper::body::Sender,
    first_error: Option<Instant>,
    last_sent: Option<Instant>,
    id: u128,
    created: Instant,
    channel: (Sender<Vec<u8>>, Receiver<Vec<u8>>),
}

impl Client {
    fn new(tx: hyper::body::Sender) -> Client {
        let mut rng = rand::rngs::ThreadRng::default();

        Client {
            tx: tx,
            first_error: None,
            last_sent: None,
            created: Instant::now(),
            id: rng.gen_range(0, 9000),
            channel: channel(1),
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Client {{ id: {} }}", self.id)
    }
}

macro_rules! send_chunk_async {
    ( $self:expr, $x:expr ) => {
        println!("check poll status");
        try_ready!($self.tx.poll_ready().map_err(|_| ()));
        println!("-> poll ok");
        $self.send_chunk(Chunk::from($x))?;
        println!("chunk sent");
    };
}

struct ImageWriterFuture {
    state: ImageWriterFutureState,
    client: QrwLock<Client>,
}

#[derive(Debug, Clone)]
enum ImageWriterFutureState {
    ReadyForNewImage,
    WrittenBoundary(Vec<u8>),
    WrittenContentType(Vec<u8>),
    WrittenContentLength(Vec<u8>),
    WrittenNewLine(Vec<u8>),
    WrittenData,
}

impl ImageWriterFuture {
    fn new(client: QrwLock<Client>) -> ImageWriterFuture {
        ImageWriterFuture {
            client: client,
            state: ImageWriterFutureState::ReadyForNewImage,
        }
    }
}

impl Future for ImageWriterFuture {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        let mut client: qutex::WriteGuard<Client> =
            try_ready!(self.client.clone().write().poll().or(Err(())));
        loop {
            match &self.state {
                ImageWriterFutureState::ReadyForNewImage => {
                    if let Some(image) = try_ready!(client.channel.1.poll()) {
                        send_chunk_async!(*client, "--BOUNDARY\r\n");
                        self.state = ImageWriterFutureState::WrittenBoundary(image)
                    } else {
                        // channel closed, no more pictures coming
                        return Ok(Async::Ready(()));
                    }
                }
                ImageWriterFutureState::WrittenBoundary(image) => {
                    send_chunk_async!(*client, "Content-Type: image/jpeg\r\n");
                    self.state = ImageWriterFutureState::WrittenContentType(image.clone())
                }
                ImageWriterFutureState::WrittenContentType(image) => {
                    send_chunk_async!(*client, format!("Content-Length: {}\r\n", image.len()));
                    self.state = ImageWriterFutureState::WrittenContentLength(image.clone());
                }
                ImageWriterFutureState::WrittenContentLength(image) => {
                    send_chunk_async!(*client, "\r\n");
                    self.state = ImageWriterFutureState::WrittenNewLine(image.clone());
                }
                ImageWriterFutureState::WrittenNewLine(image) => {
                    send_chunk_async!(*client, image.clone().clone());
                    self.state = ImageWriterFutureState::WrittenData;
                }
                ImageWriterFutureState::WrittenData => {
                    send_chunk_async!(*client, "\r\n");
                    self.state = ImageWriterFutureState::ReadyForNewImage;
                    (*client).last_sent = Some(Instant::now());
                }
            }
        }
    }
}

impl Client {
    // fn send_image_mjpeg_async(&mut self, image: Vec<u8>) -> Poll<(), ()> {
    //     try_ready!(self.channel.0.poll_ready().map_err(|_| ()));
    //     match self.channel.0.try_send(image) {
    //         Ok(_) => Ok(Async::Ready(())),
    //         Err(_) => Err(()),
    //     }
    // }

    fn send_chunk(&mut self, chunk: Chunk) -> Result<(), ()> {
        let result = self.tx.send_data(chunk);

        match (&result, self.first_error) {
            (Err(_), None) => {
                // Store time when an error was first seen
                self.first_error = Some(Instant::now());
            }
            (Ok(_), Some(_)) => {
                // Clear error when write succeeds
                self.first_error = None;
            }
            _ => {}
        }

        result.or(Err(()))
    }
}
