#![feature(exit_status_error)]
use clap::Parser;
use dialoguer::Select;
use itertools::Itertools;
use jack::{ClientOptions, PortFlags};
use konst::{primitive::parse_u64, unwrap_ctx};
use serenity::{
    async_trait,
    client::{Client, Context, EventHandler},
    framework::StandardFramework,
    model::{guild::Guild, id::UserId},
    prelude::TypeMapKey,
};
use songbird::{
    input::{Codec, Container, Input},
    Call, SerenityInit,
};
use std::{
    env,
    future::Future,
    io::Read,
    process::{Child, Command, ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

const USER_ID: UserId = UserId(unwrap_ctx!(parse_u64(env!("DISCORD_USER"))));

#[derive(Parser)]
#[clap(version, author = "Yuki Takagi <takagiy.4dev@gmail.com>")]
struct Opts {
    /// Use JACK instead of PulseAudio to listen to applications
    #[clap(short, long)]
    jack: bool,
}

struct OptsKey;

impl TypeMapKey for OptsKey {
    type Value = Opts;
}

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn guild_create(&self, ctx: Context, guild: Guild, _is_new: bool) {
        let connect_to = match guild
            .voice_states
            .get(&USER_ID)
            .and_then(|vs| vs.channel_id)
        {
            Some(channel) => channel,
            None => return,
        };

        let manager = songbird::get(&ctx)
            .await
            .expect("Failed to create voice client");

        let (_, err) = manager.join(guild.id, connect_to).await;
        err.expect("Failed to join the voice channel");
        let handler = manager
            .get(guild.id)
            .expect("Failed to obtain the connection");
        let source_device;
        {
            let data = ctx.data.read().await;
            let opts = data.get::<OptsKey>().unwrap();
            source_device = if opts.jack {
                "audst_jack"
            } else {
                "audst.monitor"
            };
        }
        play_source(handler, source_device).await;
    }
}

async fn play_source(handler: Arc<tokio::sync::Mutex<Call>>, source_device: &str) {
    let mut recorder = Command::new("parec")
        .args(&[
            "--format=float32le",
            "--rate=48000",
            "--channels=2",
            "-d",
            source_device,
            "-r",
        ])
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Failed to spawn `parec`");
    // ensure that the process is alive
    const TIMEOUT: usize = 50;
    let status = tokio::time::timeout(Duration::from_millis(TIMEOUT as u64), recorder.status());
    if let Ok(status) = status.await {
        match status {
            Ok(status) => panic!("`parec` exited with status {}", status),
            Err(e) => panic!("Failed to check `parec`'s status: {}", e),
        };
    }
    // discard accumulated bytes
    if let Some(stdout) = recorder.stdout.as_mut() {
        let mut buffer = vec![0; 48 * TIMEOUT * 2 * 4];
        let _ = stdout.read_exact(&mut buffer);
    }
    let source = Input::new(
        true,
        recorder.into(),
        Codec::FloatPcm,
        Container::Raw,
        Default::default(),
    );
    let mut handler = handler.lock().await;
    handler.play_source(source);
}

#[tokio::main]
async fn main() {
    let opts = Opts::parse();
    let mut workers = Vec::new();
    if opts.jack {
        pa_load_once(&[
            "module-jack-source",
            "source_name=audst_jack",
            "client_name=\"Audst\\ REC\"",
        ]);
        let jack = jack::Client::new("Audst", ClientOptions::NO_START_SERVER)
            .expect("Failed to create JACK client")
            .0;
        jack_disconnect_input(&jack);
        workers.push(tokio::task::spawn_blocking(move || {
            select_source_app_jack(&jack);
        }));
    } else {
        pa_load_once(&[
            "module-null-sink",
            "sink_name=audst",
            "sink_properties=device.description=\"Audst\\ REC\"",
        ]);
        pa_load_once(&[
            "module-loopback",
            "source=audst.monitor",
            "sink=@DEFAULT_SINK@",
        ]);
        workers.push(tokio::task::spawn_blocking(move || {
            select_source_app_pulse();
        }));
    };
    let token = env!("DISCORD_TOKEN");
    let framework = StandardFramework::new();
    let mut client = Client::builder(&token)
        .event_handler(Handler)
        .framework(framework)
        .register_songbird()
        .await
        .expect("Failed to create client");
    {
        let mut data = client.data.write().await;
        data.insert::<OptsKey>(opts);
    }
    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        if let Err(err) = tokio::signal::ctrl_c().await {
            println!("error: {:?}", err);
        }
        shard_manager.lock().await.shutdown_all().await;
        for worker in workers {
            if let Err(err) = worker.await {
                println!("error: {:?}", err);
            }
        }
    });
    if let Err(err) = client.start().await {
        println!("error: {:?}", err);
    }
}

fn select_source_app_jack(jack: &jack::Client) {
    loop {
        let _guard = TermGuard;
        let clients = jack.ports(None, Some("32 bit float mono audio"), PortFlags::IS_OUTPUT);
        let mut clients: Vec<_> = clients
            .iter()
            .filter_map(|name| name.split(':').next())
            .unique()
            .collect();
        clients.push("(none)");
        clients.push("(rescan)");
        let connect_to = match Select::new()
            .with_prompt("Application to stream")
            .report(false)
            .default(0)
            .items(&clients)
            .interact()
        {
            Ok(selection) => clients[selection],
            Err(_) => return,
        };
        if connect_to == "(rescan)" {
            continue;
        }
        jack_disconnect_input(jack);
        if connect_to == "(none)" {
            continue;
        }
        for (rec, src) in jack
            .ports(
                Some("^Audst REC:"),
                Some("32 bit float mono audio"),
                PortFlags::IS_INPUT,
            )
            .into_iter()
            .zip(jack.ports(
                Some(&format!("^{}:", connect_to)),
                Some("32 bit float mono audio"),
                PortFlags::IS_OUTPUT,
            ))
        {
            jack.connect_ports_by_name(&src, &rec)
                .expect("Failed to manipulate port connections");
        }
    }
}

fn select_source_app_pulse() {
    let mut prev_connected = None;
    loop {
        let _guard = TermGuard;
        let client_infos = pa_get_sink_inputs();
        let mut clients: Vec<_> = client_infos.iter().map(|info| info.1.as_str()).collect();
        clients.push("(none)");
        clients.push("(rescan)");
        let (connect_to, client_id) = match Select::new()
            .with_prompt("Application to stream")
            .report(false)
            .default(0)
            .items(&clients)
            .interact()
        {
            Ok(selection) => (clients[selection], client_infos.get(selection)),
            Err(_) => {
                if let Some(id) = prev_connected {
                    pa_move_sink_inputs(id, "@DEFAULT_SINK@");
                }
                return;
            }
        };
        if connect_to == "(rescan)" {
            continue;
        }
        if let Some(id) = prev_connected {
            pa_move_sink_inputs(id, "@DEFAULT_SINK@");
        }
        if connect_to == "(none)" {
            continue;
        }
        let connect_to = client_id.unwrap().0;
        pa_move_sink_inputs(connect_to, "audst");
        prev_connected = Some(connect_to);
    }
}

fn jack_disconnect_input(jack: &jack::Client) {
        for name in jack.ports(
            Some("^Audst REC:"),
            Some("32 bit float mono audio"),
            PortFlags::IS_INPUT,
        ) {
            if let Some(port) = jack.port_by_name(&name) {
                jack.disconnect(&port)
                    .expect("Failed to manipulate port connections");
            }
        }
}

fn pa_load_once(args: &[&str]) {
    if !pa_find_modules(args).is_empty() {
        return;
    }
    Command::new("pactl")
        .arg("load-module")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .expect("Failed to load module")
        .exit_ok()
        .expect("Failed to load module");
}

fn pa_unload_module(args: &[&str]) {
    for id in pa_find_modules(args) {
        Command::new("pactl")
            .arg("unload-module")
            .arg(&id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .expect("Failed to unload module")
            .exit_ok()
            .expect("Failed to unload module");
    }
}

fn pa_find_modules(args: &[&str]) -> Vec<String> {
    let out = Command::new("pactl")
        .args(&["list", "short", "modules"])
        .stderr(Stdio::inherit())
        .output()
        .expect("Failed to get modules");
    out.status.exit_ok().expect("Failed to get modules");
    let out = String::from_utf8(out.stdout).expect("Invalid UTF-8");
    out.lines()
        .filter(|line| args.iter().all(|arg| line.contains(arg)))
        .filter_map(|line| line.split_whitespace().nth(1).map(ToOwned::to_owned))
        .collect()
}

fn pa_get_sinks() -> Vec<String> {
    let out = Command::new("pactl")
        .args(&["list", "short", "sinks"])
        .stderr(Stdio::inherit())
        .output()
        .expect("Failed to get sinks");
    out.status.exit_ok().expect("Failed to get sinks");
    let out = String::from_utf8(out.stdout).expect("Invalid UTF-8");
    out.lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter(|name| name != &"audst")
        .map(ToOwned::to_owned)
        .collect()
}

fn pa_move_sink_inputs(app_id: usize, sink: &str) {
    Command::new("pactl")
        .args(&["move-sink-input", &app_id.to_string(), sink])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .expect("Failed to move sink input");
}

fn pa_get_sink_inputs() -> Vec<(usize, String)> {
    let out = Command::new("pacmd")
        .args(&["list-sink-inputs"])
        .stderr(Stdio::inherit())
        .output()
        .expect("Failed to get sink inpus");
    out.status.exit_ok().expect("Failed to get sink inpus");
    let out = String::from_utf8(out.stdout).expect("Invalid UTF-8");

    let mut result = Vec::new();
    let mut index = 0;
    for line in out.lines().map(str::trim) {
        if let Some(idx) = line.strip_prefix("index:") {
            index = idx.trim().parse().expect("Index of sink input is invalid");
        }
        if let Some(app) = line.strip_prefix("application.name = ") {
            result.push((index, app.trim().trim_matches('"').to_owned()));
        }
    }
    result
}

struct AsyncExitStatus<'ch> {
    child: &'ch mut Child,
}

trait ChildExt {
    fn status(&mut self) -> AsyncExitStatus;
}

impl ChildExt for Child {
    fn status(&mut self) -> AsyncExitStatus {
        AsyncExitStatus { child: self }
    }
}

impl<'ch> Future for AsyncExitStatus<'ch> {
    type Output = std::io::Result<ExitStatus>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.get_mut().child.try_wait() {
            Ok(Some(status)) => std::task::Poll::Ready(Ok(status)),
            Ok(None) => std::task::Poll::Pending,
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }
}

struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        console::Term::stdout().show_cursor().unwrap();
        console::Term::stderr().show_cursor().unwrap();
    }
}
