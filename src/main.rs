use actix_web::error;
use actix_web::{middleware, web, App, Error, HttpResponse, HttpServer};
use awc::Client;
use html5ever;
use html5ever::rcdom::{Node, NodeData};
use html5ever::tendril::TendrilSink;

use futures::future::ok;
use futures::stream;
use futures::sync::mpsc;
use futures::{Future, Sink, Stream};

use log::{debug, info};
use serde::Deserialize;
use url::{Host, Url};

use std::collections::{BTreeMap as Map, BTreeSet as Set};
use std::sync::Mutex;

#[derive(Debug, Clone, Deserialize)]
struct Crawl {
    root_url: Url,
}

#[derive(Debug)]
enum ScrapeRequest {
    Root(Url),
    Scraped { page: Url, found: Vec<Url> },
}

// TODO use a futex?
type StateMap = web::Data<Mutex<Map<Host, Set<Url>>>>;

fn fetch_url_and_scrape(
    client: &Client,
    url: Url,
) -> impl Future<Item = (Url, Vec<Url>), Error = Error> {
    client
        .get(url.as_str())
        .send()
        .map_err(Error::from)
        .and_then(|mut resp| resp.body().map_err(Error::from))
        .map(move |bytes| {
            let found = find_anchor_links(&url, &bytes);
            (url, found)
        })
}

fn find_anchor_links(this: &Url, html: &[u8]) -> Vec<Url> {
    fn walk(node: &Node, urls: &mut Vec<Url>, this: &Url) {
        if let NodeData::Element {
            ref name,
            ref attrs,
            ..
        } = node.data
        {
            if &*name.local == "a" {
                if let Some(href) = attrs
                    .borrow()
                    .iter()
                    .find(|attr| &*attr.name.local == "href")
                {
                    match Url::parse(&*href.value) {
                        Ok(url) => urls.push(url),
                        Err(_) => {
                            // try joining to current url
                            if let Ok(url) = this.join(&*href.value) {
                                urls.push(url)
                            } // else it's a bad url/not a url/whatever
                        }
                    }
                }
            }
        }
        for child in node.children.borrow().iter() {
            walk(child, urls, this)
        }
    }

    let mut r = std::io::Cursor::new(html);
    let dom =
        match html5ever::parse_document(html5ever::rcdom::RcDom::default(), Default::default())
            .from_utf8()
            .read_from(&mut r)
        {
            Ok(dom) => dom,
            Err(_e) => return vec![],
        };
    let mut rtn = Vec::new();
    walk(&*dom.document, &mut rtn, this);
    rtn
}

// We have request to scrape some urls, but we might have already done so
fn urls_to_scrape(request: ScrapeRequest, map: &StateMap) -> Vec<Url> {
    debug!("Scrape request: {:?}", request);
    match request {
        ScrapeRequest::Root(url) => {
            // We have a new url crawl request (from the webserver)
            let host = match url.host() {
                Some(h) => h.to_owned(),
                None => return vec![],
            };
            let mut map = map.lock().unwrap();
            if !map.contains_key(&host) {
                // If we haven't seen the domain before, start scraping
                map.insert(host, Set::new());
                vec![url]
            } else {
                debug!("Already seen: {}", url);
                vec![]
            }
        }
        ScrapeRequest::Scraped { page, found } => {
            // We have the results of an ongoing domain scrape
            let host = match page.host() {
                Some(h) => h.to_owned(),
                None => return vec![],
            };
            let mut map = map.lock().unwrap();
            let domain_urls = map.get_mut(&host).expect("domain not found");
            found
                .into_iter()
                .filter_map(move |url| {
                    // stash the url with the domain as a key
                    if domain_urls.insert(url.clone())
                        && url.host().map(|h| h.to_owned()).as_ref() == Some(&host)
                    {
                        info!("Url count: {}", domain_urls.len());
                        // We haven't seen this url before, so scrape it
                        Some(url)
                    } else {
                        // the url is not in the domain, or we have seen it before
                        // do nothing
                        None
                    }
                })
                .collect()
        }
    }
}

fn client() -> Client {
    let ssl= openssl::ssl::SslConnector::builder(openssl::ssl::SslMethod::tls()).unwrap().build();
    let connector = awc::Connector::new()
        .ssl(ssl)
        .finish();
    // TODO extend timeout, at moment it fails easily
    Client::build().connector(connector).finish()
}

fn scrape_urls(
    queue_rx: mpsc::UnboundedReceiver<ScrapeRequest>,
    queue_tx: mpsc::UnboundedSender<ScrapeRequest>,
    map: StateMap,
) -> impl Future<Item = (), Error = ()> {
    // The logic here is pretty fiddly.
    // To be honest it would have been much easier just to spawn separate threads for
    // each crawler request, but, we have these shiny async things now so we may as
    // well use them.
    //
    // It would be interesting to have triend this with `async/await` but I didn't
    // fancy trying to mix it with `actix-web` which is still running `futures 0.1`.
    // Also stream don't work with await yet.

    let client = client();

    // This future will forever (or until nasty segfault)
    queue_rx
        .map(move |request| {
            let urls = urls_to_scrape(request, &map);
            let client = client.clone();
            stream::iter_ok(
                urls.into_iter()
                    .map(move |url| fetch_url_and_scrape(&client, url)),
            )
        })
        .flatten()
        // now we have a stream of futures, each future is a client request
        // we want to limit the number of requests in flight at once
        .buffer_unordered(500)
        // to make sure it runs forever, we must intercept errors at this point
        .then(|res| {
            ok(match res {
                Ok(ok) => Some(ok),
                Err(e) => {
                    info!("scrape error: {}", e);
                    None
                }
            })
        })
        .filter_map(|opt| opt)
        .for_each(move |(page, found)| {
            let req = ScrapeRequest::Scraped { page, found };
            queue_tx.clone().send(req).then(|res| {
                if let Err(e) = res {
                    info!("tx error: {}", e)
                };
                // We need to intercept errors here else they will cancel rest
                // of the stream
                ok(())
            })
        })
}

fn start_crawl(
    web::Json(crawl): web::Json<Crawl>,
    client: web::Data<Client>,
    queue_tx: web::Data<mpsc::UnboundedSender<ScrapeRequest>>,
) -> Box<dyn Future<Item = HttpResponse, Error = Error>> {
    debug!("Crawl request: {:?}", crawl);

    // check it is a 'bare' domain url
    if crawl.root_url.path() != "/" {
        return Box::new(ok(HttpResponse::BadRequest().body("Not a domain")));
    }

    // check the url exists
    let q = (*queue_tx).clone();
    let resp = client
        .get(crawl.root_url.as_str())
        .send()
        .map_err(Error::from)
        .and_then(move |_resp| {
            // send the url to the background thread
            q.send(ScrapeRequest::Root(crawl.root_url))
                .map_err(error::ErrorInternalServerError)
        })
        .and_then(|_tx| {
            // crawl in progress - return success
            HttpResponse::Ok().finish()
        });
    Box::new(resp)
}

fn all_domain_urls(web::Json(lookup): web::Json<Crawl>, map: StateMap) -> HttpResponse {
    let map = map.lock().unwrap();
    if lookup.root_url.path() != "/" {
        return HttpResponse::BadRequest().body("Not a domain");
    }
    let host = match lookup.root_url.host() {
        Some(host) => host.to_owned(),
        None => return HttpResponse::BadRequest().body("Not a domain"),
    };
    if let Some(urls) = map.get(&host) {
        let urls: Vec<_> = urls.iter().collect();
        HttpResponse::Ok().json(urls)
    } else {
        HttpResponse::NotFound().body("Domain not yet scraped")
    }
}

// copy pasta
fn domain_count(web::Json(lookup): web::Json<Crawl>, map: StateMap) -> HttpResponse {
    let map = map.lock().unwrap();
    if lookup.root_url.path() != "/" {
        return HttpResponse::BadRequest().body("Not a domain");
    }
    let host = match lookup.root_url.host() {
        Some(host) => host.to_owned(),
        None => return HttpResponse::BadRequest().body("Not a domain"),
    };
    if let Some(urls) = map.get(&host) {
        HttpResponse::Ok().body(format!("{}", urls.len()))
    } else {
        HttpResponse::NotFound().body("Domain not yet scraped")
    }
}

fn main() {
    env_logger::init();

    let map: StateMap = web::Data::new(Mutex::new(Map::new()));
    let (tx, rx) = mpsc::unbounded();
    let queue_tx = web::Data::new(tx.clone());

    let sys = actix_rt::System::new("crawling-in-my-skin");

    // Spawn the scraping 'background task' on the main thread.
    // Potentially this task could run on many threads
    let scrape_fut = scrape_urls(rx, tx, map.clone());
    actix_rt::spawn(scrape_fut);

    HttpServer::new(move || {
        App::new()
            .wrap(middleware::Logger::default())
            .register_data(map.clone())
            .register_data(queue_tx.clone())
            .data(client())
            .route("/crawl", web::post().to_async(start_crawl))
            .route("/domain-urls", web::post().to(all_domain_urls))
            .route("/domain-count", web::post().to(domain_count))
    })
    .bind("127.0.0.1:8000")
    .unwrap()
    .start();

    // Start the server
    sys.run().unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_scrape() {
        let html = std::fs::read_to_string("rustlang.html").unwrap();
        let url = Url::parse("https://www.rust-lang.org").unwrap();
        let links = find_anchor_links(&url, html.as_bytes());
        assert_eq!(links.len(), 37);
    }
}
