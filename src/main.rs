use std::{
    collections::HashMap,
    sync::atomic::Ordering,
    sync::{atomic::AtomicUsize, Arc},
    time::{Duration, Instant},
};

use actix::prelude::*;
use actix_files::NamedFile;
use actix_web::{middleware, web, App, Error, HttpRequest, HttpResponse, HttpServer, Responder};
use actix_web_actors::ws;
use rand::{rngs::ThreadRng, Rng};
use serde::Serialize;

async fn index() -> impl Responder {
    NamedFile::open_async("./static/index.html").await.unwrap()
}

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Message)]
#[rtype(result = "()")]
pub struct Message(pub String);

#[derive(Message)]
#[rtype(Domino)]
pub struct Topple;

#[derive(Message)]
#[rtype(Domino)]
pub struct Setup;

#[derive(Message)]
#[rtype(usize)]
pub struct Connect {
    pub addr: Recipient<Message>,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Disconnect {
    pub id: usize,
}

#[derive(Debug, Clone, Serialize, MessageResponse)]
pub struct Domino {
    domino: usize,
    high: usize,
}

pub struct DominoServer {
    sessions: HashMap<usize, Recipient<Message>>,
    rng: ThreadRng,
    domino_count: Arc<AtomicUsize>,
    high: Arc<AtomicUsize>,
}

impl DominoServer {
    pub fn new(domino_count: Arc<AtomicUsize>, high: Arc<AtomicUsize>) -> Self {
        DominoServer {
            sessions: HashMap::new(),
            rng: rand::thread_rng(),
            domino_count,
            high,
        }
    }
}

impl Actor for DominoServer {
    type Context = Context<Self>;
}

impl Handler<Connect> for DominoServer {
    type Result = usize;

    fn handle(&mut self, msg: Connect, _: &mut Context<Self>) -> Self::Result {
        println!("Someone joined");

        let id = self.rng.gen::<usize>();
        self.sessions.insert(id, msg.addr);

        id
    }
}

impl Handler<Disconnect> for DominoServer {
    type Result = ();

    fn handle(&mut self, msg: Disconnect, _: &mut Context<Self>) {
        println!("Someone disconnected");

        self.sessions.remove(&msg.id);
    }
}

impl Handler<Setup> for DominoServer {
    type Result = Domino;

    fn handle(&mut self, _msg: Setup, _ctx: &mut Self::Context) -> Self::Result {
        let domino = self.domino_count.fetch_add(1, Ordering::SeqCst) + 1;
        let mut high = self.high.load(Ordering::SeqCst);
        if high < domino {
            self.high.store(domino, Ordering::SeqCst);
            high = domino;
        }

        Domino { domino, high }
    }
}

impl Handler<Topple> for DominoServer {
    type Result = Domino;

    fn handle(&mut self, _msg: Topple, _ctx: &mut Self::Context) -> Self::Result {
        self.domino_count.store(0, Ordering::SeqCst);
        Domino {
            domino: self.domino_count.load(Ordering::SeqCst),
            high: self.high.load(Ordering::SeqCst),
        }
    }
}

pub struct WsSession {
    pub id: usize,
    pub hb: Instant,
    pub addr: Addr<DominoServer>,
}

impl WsSession {
    fn hb(&self, ctx: &mut ws::WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.hb) > CLIENT_TIMEOUT {
                println!("Websocket Client heartbeat failed, disconnecting!");
                ctx.stop();
                return;
            }

            ctx.ping(b"");
        });
    }
}

impl Actor for WsSession {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.hb(ctx);
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsSession {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        // process websocket messages
        println!("WS: {msg:?}");
        match msg {
            Ok(ws::Message::Ping(msg)) => {
                self.hb = Instant::now();
                ctx.pong(&msg);
            }
            Ok(ws::Message::Pong(_)) => {
                self.hb = Instant::now();
            }
            Ok(ws::Message::Text(text)) => {
                if text.starts_with("setup") {
                    self.addr
                        .send(Setup)
                        .into_actor(self)
                        .then(|res, _, ctx| {
                            ctx.text(serde_json::to_string(&res.unwrap()).unwrap());
                            fut::ready(())
                        })
                        .wait(ctx)
                } else if text.starts_with("topple") {
                    self.addr
                        .send(Topple)
                        .into_actor(self)
                        .then(|res, _, ctx| {
                            ctx.text(serde_json::to_string(&res.unwrap()).unwrap());
                            fut::ready(())
                        })
                        .wait(ctx)
                }
            }
            Ok(ws::Message::Binary(bin)) => ctx.binary(bin),
            Ok(ws::Message::Close(reason)) => {
                ctx.close(reason);
                ctx.stop();
            }
            _ => ctx.stop(),
        }
    }
}

async fn domino_route(
    req: HttpRequest,
    stream: web::Payload,
    srv: web::Data<Addr<DominoServer>>,
) -> Result<HttpResponse, Error> {
    ws::start(
        WsSession {
            id: 0,
            hb: Instant::now(),
            addr: srv.get_ref().clone(),
        },
        &req,
        stream,
    )
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
    log::info!("starting HTTP server at http://localhost:8080");

    let domino_count = Arc::new(AtomicUsize::new(0));
    let high = Arc::new(AtomicUsize::new(0));
    let server = DominoServer::new(domino_count.clone(), high.clone()).start();

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::from(domino_count.clone()))
            .app_data(web::Data::from(high.clone()))
            .app_data(web::Data::new(server.clone()))
            .service(web::resource("/").to(index))
            .service(web::resource("/ws").route(web::get().to(domino_route)))
            .wrap(middleware::Logger::default())
    })
    .workers(2)
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}
