use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use actix::prelude::*;
use actix_files::NamedFile;
use actix_web::{middleware, web, App, Error, HttpRequest, HttpResponse, HttpServer, Responder};
use actix_web_actors::ws;
use rand::{rngs::ThreadRng, Rng};
use serde::{Deserialize, Serialize};
use yrs::{Doc, Map, ReadTxn, Transact};

async fn index() -> impl Responder {
    NamedFile::open_async("./static/index.html").await.unwrap()
}

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct UpdateRequest {
    pub state: Domino,
    pub id: usize,
}

#[derive(Message)]
#[rtype(usize)]
pub struct Connect {
    pub addr: Recipient<UpdateRequest>,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Disconnect {
    pub id: usize,
}

// game state
#[derive(Debug, Clone, Serialize, Deserialize, MessageResponse)]
pub struct Domino {
    id: usize,
    domino: i64,
    high: i64,
}

// definitions of server
pub struct DominoServer {
    sessions: HashMap<usize, Recipient<UpdateRequest>>,
    rng: ThreadRng,
    state: Arc<Doc>,
}

impl DominoServer {
    pub fn new(state: Arc<Doc>) -> Self {
        DominoServer {
            sessions: HashMap::new(),
            rng: rand::thread_rng(),
            state,
        }
    }

    pub fn send_update(&self) {
        for (id, recp) in self.sessions.clone().into_iter() {
            let doc = self.state.clone();
            let txn = doc.transact();
            let map = txn.get_map("state").unwrap();
            recp.do_send(UpdateRequest {
                id,
                state: Domino {
                    id,
                    domino: map.get(&txn, "domino").unwrap().try_into().unwrap(),
                    high: map.get(&txn, "high").unwrap().try_into().unwrap(),
                },
            });
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
        self.send_update();

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

impl Handler<UpdateRequest> for DominoServer {
    type Result = ();

    fn handle(&mut self, msg: UpdateRequest, _ctx: &mut Self::Context) -> Self::Result {
        {
            let root_doc = self.state.clone();
            let map = root_doc.get_or_insert_map("state");
            let mut root_txn = root_doc.transact_mut();
            map.insert(&mut root_txn, "domino", msg.state.domino);
            map.insert(&mut root_txn, "high", msg.state.high);
        }
        self.send_update();
    }
}

// definitions of session
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
        println!("start session");
        self.hb(ctx);

        let addr = ctx.address();
        self.addr
            .send(Connect {
                addr: addr.recipient(),
            })
            .into_actor(self)
            .then(|res, act, ctx| {
                match res {
                    Ok(r) => act.id = r,
                    _ => ctx.stop(),
                }
                fut::ready(())
            })
            .wait(ctx);
    }

    fn stopping(&mut self, _ctx: &mut Self::Context) -> Running {
        self.addr.do_send(Disconnect { id: self.id });
        Running::Stop
    }
}

impl Handler<UpdateRequest> for WsSession {
    type Result = ();

    fn handle(&mut self, msg: UpdateRequest, ctx: &mut Self::Context) -> Self::Result {
        ctx.text(serde_json::to_string(&msg.state).unwrap());
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsSession {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        println!("WS: {:?}", msg);
        match msg {
            Ok(ws::Message::Ping(msg)) => {
                self.hb = Instant::now();
                ctx.pong(&msg);
            }
            Ok(ws::Message::Pong(_)) => {
                self.hb = Instant::now();
            }
            Ok(ws::Message::Text(text)) => {
                let state: Domino = serde_json::from_str(&text).unwrap();
                println!("{:?}", state);
                self.addr.do_send(UpdateRequest { id: self.id, state });
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

    // prepare the root of document
    let root_doc = Arc::new(Doc::new());
    {
        let doc = root_doc.clone();
        let map = doc.get_or_insert_map("state");
        let mut txn = doc.transact_mut();
        map.insert(&mut txn, "domino", 0);
        map.insert(&mut txn, "high", 0);
    } // transaction is committed automatically when it is dropped
    let server = DominoServer::new(root_doc.clone()).start();

    HttpServer::new(move || {
        App::new()
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
