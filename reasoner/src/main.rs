use warp::Filter;
use std::str;
use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use warp::http::Response;
use tokio::time;
use futures::channel::mpsc;
use futures::{
    select, future, join, pin_mut, stream, try_join, FutureExt, SinkExt, StreamExt, TryStreamExt,
};
use std::io::BufWriter;
use rdf::{node::Node, uri::Uri};
use tokio_postgres::{NoTls, Error, AsyncMessage};
use oxigraph::sparql::{QueryOptions, QueryResults, QueryResultsFormat};
use oxigraph::MemoryStore;
use reasonable::manager::Manager;

#[allow(non_upper_case_globals)]
const qfmt: &str = "PREFIX brick: <https://brickschema.org/schema/1.1/Brick#>
    PREFIX tag: <https://brickschema.org/schema/1.1/BrickTag#>
    PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
    PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
    PREFIX owl: <http://www.w3.org/2002/07/owl#>
    PREFIX qudt: <http://qudt.org/schema/qudt/>
    ";

fn with_db(store: MemoryStore) -> impl Filter<Extract = (MemoryStore,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || store.clone())
}

#[derive(Serialize, Deserialize, Debug)]
struct EmbeddedTriple {
    s: String,
    p: String,
    o: String,
}
#[derive(Serialize, Deserialize, Debug)]
struct TripleEvent {
    table: String,
    action: String,
    data: EmbeddedTriple
}

#[derive(Debug)]
struct NotUtf8;
impl warp::reject::Reject for NotUtf8 {}

fn parse_triple_term(t: &str) -> Option<Node> {
    let r = match t.as_bytes()[0] as char {
        '<' => Node::UriNode { uri: Uri::new(t.replace(&['<','>'][..], "")) },
        '_' => Node::BlankNode { id: t.to_string() },
        '"' => Node::LiteralNode { literal: t.to_string(), data_type: None, language: None },
        _ => return None
    };
    Some(r)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut mgr = Manager::new();
    // mgr.load_file("/home/gabe/src/Brick/Brick/Brick.ttl").unwrap();
    // mgr.load_file("/home/gabe/src/Brick/Brick/examples/soda_brick.ttl").unwrap();
    //mgr.load_file("~/src/Brick/Brick/examples/soda_brick.ttl").unwrap();
    // let store = mgr.store();
    let store = mgr.store();
    println!("Loaded files");

    // Connect to the database.
    let (client, mut connection) =
        tokio_postgres::connect("host=localhost port=5434 dbname=mortar user=mortarchangeme password=mortarpasswordchangeme", NoTls).await?;

    let (tx, mut rx) = mpsc::unbounded();
    let stream = stream::poll_fn(move |cx| connection.poll_message(cx)).map_err(|e| panic!(e));
    let connection = stream.forward(tx).map(|r| r.unwrap());

    // The connection object performs the actual communication with the database,
    // so spawn it off to run on its own.
    tokio::spawn(async move {
        connection.await;
        // if let Err(e) = connection.await {
        //     eprintln!("connection error: {}", e);
        // }
    });

    let rows = client.query("SELECT s, p, o FROM latest_triples", &[]).await?;
    let v: Vec<(Node, Node, Node)> = rows.iter().filter_map(|row| {
        let (s, p, o): (&str, &str, &str) = (row.get(0), row.get(1), row.get(2));
        Some((parse_triple_term(s)?,
              parse_triple_term(p)?,
              parse_triple_term(o)?))
    }).collect();
    println!("triples: {}", v.len());
    mgr.add_triples(v);


    // subscribe
    client.execute("LISTEN events;", &[]).await?;

    // let notifications = rx
    //     .filter_map(|m| match m {
    //             AsyncMessage::Notification(n) => future::ready(Some(n)),
    //             _ => future::ready(None),
    //         })
    //     .collect::<Vec<_>>()
    //     .await;


    // TODO: how to listen for changes?
    // 1) bootstrap content by querying PG table (using
    //    https://docs.rs/tokio-postgres/0.5.5/tokio_postgres/)
    // 2) maybe use LISTEN/NOTIFY
    //    (https://citizen428.net/blog/asynchronous-notifications-in-postgres/). Would just need to
    //    execute the `LISTEN channame;` in a client and then do client.notifications() or
    //    something like that

    let query = warp::path!("query")
            .and(warp::body::content_length_limit(1024))
            .and(
                warp::body::bytes().and_then(|body: bytes::Bytes| async move {
                    std::str::from_utf8(&body)
                        .map(String::from)
                        .map_err(|_e| warp::reject::custom(NotUtf8))
                }),
            )
            .and(with_db(store.clone()))
            .map(|query: String, store: MemoryStore| {
                let sparql = format!("{}{}", qfmt, query);
                println!("query: {}", sparql);
                let q = store.clone().prepare_query(&sparql, QueryOptions::default()).unwrap();
                let res = q.exec().unwrap();
                let mut resp: Vec<u8> = Vec::new();
                if let QueryResults::Solutions(_) = res {
                    res.write(&mut resp, QueryResultsFormat::Json).unwrap();
                    warp::http::Response::builder()
                        .header("content-type", "application/json")
                        .body(resp)
                } else {
                    warp::http::Response::builder()
                        .status(warp::http::StatusCode::INTERNAL_SERVER_ERROR)
                        .body("No results".as_bytes().to_vec())

                }
            });

    let query2 = warp::path!("query")
            .and(warp::body::content_length_limit(1024))
            .and(warp::header::exact("content-type", "application/x-www-form-urlencoded"))
            .and(warp::body::form())
            .and(with_db(store.clone()))
            .map(|m: HashMap<String, String>, store: MemoryStore| {
                if let Some(query) = m.get("query") {
                    let sparql = format!("{}{}", qfmt, query);
                    println!("query: {}", sparql);
                    let q = store.clone().prepare_query(&sparql, QueryOptions::default()).unwrap();
                    let res = q.exec().unwrap();
                    let mut resp: Vec<u8> = Vec::new();
                    if let QueryResults::Solutions(_) = res {
                        res.write(&mut resp, QueryResultsFormat::Json).unwrap();
                        warp::http::Response::builder()
                            .header("content-type", "application/json")
                            .body(resp)
                    } else {
                        warp::http::Response::builder()
                            .status(warp::http::StatusCode::INTERNAL_SERVER_ERROR)
                            .body("No results".as_bytes().to_vec())

                    }
                } else {
                    warp::http::Response::builder()
                        .status(warp::http::StatusCode::INTERNAL_SERVER_ERROR)
                        .body("Bad query".as_bytes().to_vec())

                }
            });

    println!("Serving on 127.0.0.1:3030");
    tokio::spawn(
        warp::serve(query2.or(query))
            .run(([127, 0, 0, 1], 3030))
    );

    let mut trips: Vec<(Node, Node, Node)> = Vec::new();
    let mut interval = time::interval(time::Duration::from_secs(10));
    loop {
        tokio::select! {
            msg = rx.next() => {
                if let Some(x) = msg {
                    if let AsyncMessage::Notification(n) = x {
                        let msg: TripleEvent = serde_json::from_str(n.payload()).unwrap();
                        trips.push((parse_triple_term(&msg.data.s).unwrap(),
                                    parse_triple_term(&msg.data.p).unwrap(),
                                    parse_triple_term(&msg.data.o).unwrap()));
                    }
                }
            },
            _ = interval.tick() => {
                if trips.len() > 0 {
                    mgr.add_triples(trips.clone());
                    println!("Integrated {}", trips.len());
                    trips.clear();
                }
            }
        }
    }

    Ok(())
}
