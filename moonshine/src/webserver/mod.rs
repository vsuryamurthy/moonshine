use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::collections::HashMap;

use hyper::Uri;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::net::TcpListener;
use hyper::{Response, Body, header::CONTENT_TYPE, Request, Method, StatusCode};

mod pairing;
use pairing::Clients;

mod tls;
use tls::TlsAcceptor;

use crate::config;
use crate::util::flatten;

type Params = HashMap<String, String>;

pub(crate) async fn run(config: config::Config) -> Result<(), ()> {
	let clients = Clients::from_state_or_default();

	let http_task = tokio::spawn(run_http_server(config.clone(), clients.clone()));
	log::info!("Http server listening on '{}:{}'", config.address, config.port);

	let https_task = tokio::spawn(run_https_server(config.clone(), clients.clone()));
	log::info!("Https server listening on '{}:{}'", config.address, config.tls.port);

	let result = tokio::try_join!(flatten(http_task), flatten(https_task));
	match result {
		Ok(_) => {
			log::info!("Finished without errors.");
			Ok(())
		},
		Err(_) => {
			log::error!("Finished with errors.");
			Err(())
		}
	}
}

async fn run_http_server(
	config: config::Config,
	clients: Clients,
) -> Result<(), ()> {
	let http_address = (config.address.clone(), config.port).to_socket_addrs()
		.map_err(|e| log::error!("Failed to resolve address '{}:{}': {}", config.address, config.port, e))?
		.next()
		.ok_or_else(|| log::error!("Failed to resolve address '{}:{}'", config.address, config.port))?;
	let listener = TcpListener::bind(http_address)
		.await
		.map_err(|e| log::error!("Failed to bind to address '{}': {}", http_address, e))?;

	loop {
		let (connection, address) = listener.accept()
			.await
			.map_err(|e| log::error!("Failed to accept connection: {}", e))?;
		log::debug!("Accepted connection from {}", address);

		tokio::spawn(handle_connection(connection, address, config.clone(), clients.clone()));
	}
}

async fn run_https_server(
	config: config::Config,
	clients: Clients,
) -> Result<(), ()> {
	let https_address = (config.address.clone(), config.tls.port).to_socket_addrs()
		.map_err(|e| log::error!("No address resolved for '{}:{}': {}", config.address, config.tls.port, e))?
		.next()
		.ok_or_else(|| log::error!("No address resolved for {}:{}", config.address, config.tls.port))?;

	let listener = TcpListener::bind(https_address)
		.await
		.map_err(|e| log::error!("Failed to bind to {}: {}", https_address, e))?;
	let acceptor = TlsAcceptor::from_config(&config.tls)?;

	loop {
		let (connection, address) = listener.accept()
			.await
			.map_err(|e| log::error!("Failed to accept TLS connection: {}", e))?;
		log::debug!("Accepted TLS connection from {}", address);

		let connection = acceptor.accept(connection).await?;
		tokio::spawn(handle_connection(connection, address, config.clone(), clients.clone()));
	}
}

async fn handle_connection<C>(connection: C, address: SocketAddr, config: config::Config, clients: Clients) -> Result<(), ()>
where
	C: AsyncRead + AsyncWrite + Unpin + 'static,
{
	let result = hyper::server::conn::Http::new()
		.serve_connection(connection, hyper::service::service_fn(move |request| {
			let clients = clients.clone();
			let config = config.clone();
			async move {
				Ok::<_, String>(serve(request, config, clients).await)
			}
		}))
		.await;

	match result {
		Err(e) => {
			let message = e.to_string();
			if !message.starts_with("error shutting down connection:") {
				log::error!("Error in connection with {}: {}", address, message);
			}

			Err(())
		},
		Ok(()) => {
			Ok(())
		}
	}
}

async fn serve(req: Request<Body>, config: config::Config, clients: Clients) -> Response<Body> {
	log::info!("{} '{}' request.", req.method(), req.uri().path());

	match (req.method(), req.uri().path()) {
		(&Method::GET, "/applist") => app_list(req, config, clients).await,
		(&Method::GET, "/pair") => clients.pair(req).await,
		(&Method::GET, "/pin") => clients.pin(req).await,
		(&Method::GET, "/serverinfo") => server_info(req, config, clients).await,
		(&Method::GET, "/unpair") => clients.unpair(req).await,
		_ => not_found()
	}
}

async fn server_info(req: Request<Body>, config: config::Config, clients: Clients) -> Response<Body> {
	let params = parse_params(req.uri());

	let unique_id = match params.get("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			log::error!("Expected 'uniqueid' in pin request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	let paired = if clients.has_client(unique_id).await {
		"1"
	} else {
		"0"
	};

	let mut response = Response::new(Body::from(format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>
<root status_code=\"200\">
	<hostname>{}</hostname>
	<appversion>7.1.431.0</appversion>
	<GfeVersion>3.23.0.74</GfeVersion>
	<uniqueid>7AD14F7C-2F8B-7329-AF86-42A06F6471FE</uniqueid>
	<HttpsPort>{}</HttpsPort>
	<ExternalPort>{}</ExternalPort>
	<mac>64:bc:58:be:e5:88</mac>
	<MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>
	<LocalIP>10.0.5.137</LocalIP>
	<ServerCodecModeSupport>259</ServerCodecModeSupport>
	<SupportedDisplayMode>
		<DisplayMode>
			<Width>2560</Width>
			<Height>1440</Height>
			<RefreshRate>120</RefreshRate>
		</DisplayMode>
	</SupportedDisplayMode>
	<PairStatus>{}</PairStatus>
	<currentgame>0</currentgame>
	<state>MOONSHINE_SERVER_FREE</state>
</root>",
		config.name,
		config.tls.port,
		config.port,
		paired,
	)));
	response.headers_mut().insert(CONTENT_TYPE, "application/xml".parse().unwrap());

	response
}

async fn app_list(req: Request<Body>, config: config::Config, clients: Clients) -> Response<Body> {
	let params = parse_params(req.uri());

	let unique_id = match params.get("uniqueid") {
		Some(unique_id) => unique_id,
		None => {
			log::error!("Expected 'uniqueid' in pin request, got {:?}.", params.keys());
			return bad_request();
		}
	};

	if !clients.has_client(unique_id).await {
		log::warn!("Unknown unique id '{}' received in /applist.", unique_id);
		return bad_request();
	}

	let mut response = "<?xml version=\"1.0\" encoding=\"utf-8\"?>
<root status_code=\"200\">".to_string();

	for (i, application) in config.applications.iter().enumerate() {
		response += &format!("	<App>
		<IsHdrSupported>0</IsHdrSupported>
		<AppTitle>{}</AppTitle>
		<ID>{}</ID>
	</App>\n", application.title, i);
	}
	response += "</root>";

	let mut response = Response::new(Body::from(response));
	response.headers_mut().insert(CONTENT_TYPE, "application/xml".parse().unwrap());

	response
}

fn parse_params(uri: &Uri) -> Params {
	uri
		.query()
		.map(|v| {
			url::form_urlencoded::parse(v.as_bytes())
				.into_owned()
				.collect()
		})
		.unwrap_or_else(HashMap::new)
}

fn bad_request() -> Response<Body> {
	Response::builder()
		.status(StatusCode::BAD_REQUEST)
		.body(Body::from("BAD REQUEST".to_string()))
		.unwrap()
}

fn not_found() -> Response<Body> {
	Response::builder()
		.status(StatusCode::NOT_FOUND)
		.body(Body::from("NOT FOUND".to_string()))
		.unwrap()
}