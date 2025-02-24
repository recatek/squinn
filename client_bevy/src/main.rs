use std::error::Error;
use std::io::Error as IoError;
use std::num::ParseIntError;
use std::task::Poll;

use bevy::asset::io::Reader;
use bevy::asset::{AssetLoader, LoadContext, LoadState};
use bevy::prelude::*;

use bevy_async_task::AsyncTaskRunner;
use url::Url;
use web_transport::ClientBuilder;

const SERVER_URL: &str = "https://127.0.0.1:4443";

#[cfg(target_arch = "wasm32")]
const CERT_HEX_PATH: &str = "../cert/localhost.hex";
#[cfg(not(target_arch = "wasm32"))]
const CERT_HEX_PATH: &str = "../../cert/localhost.hex";

#[derive(Resource)]
struct Client {
    cert: Handle<StringAsset>,
}

#[derive(Asset, TypePath, Debug)]
struct StringAsset(String);

#[derive(Default)]
struct StringAssetLoader;

impl AssetLoader for StringAssetLoader {
    type Asset = StringAsset;
    type Settings = ();
    type Error = IoError;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &(),
        _load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;

        Ok(StringAsset(String::from_utf8(bytes).unwrap()))
    }
}

async fn run_client(cert_hex_str: String) -> Result<(), Box<dyn Error + Send + Sync>> {
    async {
        let hashes = vec![decode_hex(cert_hex_str.trim_end()).expect("failed to decode hex")];
        let client = ClientBuilder::new()
            .with_unreliable(true)
            .with_server_certificate_hashes(hashes)
            .expect("failed to create server");
        let server_url = Url::parse(SERVER_URL).expect("failed to parse server url");

        let mut session = client.connect(&server_url).await?;
        info!("connected");

        let msg = "hello world".to_string();

        info!("send: {}", msg);
        session.send_datagram(msg.into()).await?;

        let recv = session.recv_datagram().await?;
        info!("recv: {}", String::from_utf8_lossy(&recv));

        Ok(())
    }
    .await
    .map_err(|e: Box<dyn Error>| e.to_string().into())
}

fn client_system(
    mut running: Local<bool>,
    client: Res<Client>,
    assets: Res<AssetServer>,
    strings: Res<Assets<StringAsset>>,
    mut task_runner: AsyncTaskRunner<'_, Result<(), Box<dyn Error + Send + Sync>>>,
) {
    if let Err(err) = check_load_state(assets.load_state(&client.cert)) {
        panic!("failed to load localhost.hex: {:?}", err);
    }

    if *running == false && task_runner.is_idle() {
        if let Some(cert) = strings.get(&client.cert) {
            task_runner.start(run_client(cert.0.clone()));
            *running = true;
            info!("started");
        }
    }

    match task_runner.poll() {
        Poll::Ready(Ok(_)) => info!("done"),
        Poll::Ready(Err(e)) => error!("err: {e}"),
        Poll::Pending => { /* waiting */ }
    }
}

fn load_certs(mut commands: Commands, assets: Res<AssetServer>) {
    let handle: Handle<StringAsset> = assets.load(CERT_HEX_PATH);
    commands.insert_resource(Client { cert: handle });
}

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins)
    .add_systems(Startup, load_certs)
    .add_systems(Update, client_system)
    .init_asset::<StringAsset>()
    .init_asset_loader::<StringAssetLoader>();
    app.run();
}

fn check_load_state(state: LoadState) -> Result<(), Box<dyn Error + Send + Sync>> {
    match state {
        LoadState::Loaded | LoadState::Loading => Ok(()),
        LoadState::Failed(e) => Err(e.as_ref().clone())?,
        LoadState::NotLoaded => Err("could not find file at path")?,
    }
}

fn decode_hex(s: &str) -> Result<Vec<u8>, ParseIntError> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
        .collect()
}
