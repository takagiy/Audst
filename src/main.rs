#![feature(exit_status_error)]
use dialoguer::Select;
use konst::{primitive::parse_u64, unwrap_ctx};
use serenity::{
    async_trait,
    client::{Client, Context, EventHandler},
    framework::StandardFramework,
    model::{guild::Guild, id::UserId},
};
use songbird::{
    input::{Codec, Container, Input},
    SerenityInit,
};
use std::{
    env,
    future::Future,
    io::Read,
    process::{Child, Command, ExitStatus, Stdio},
    time::Duration,
};

const USER_ID: UserId = UserId(unwrap_ctx!(parse_u64(env!("DISCORD_USER"))));

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
        let mut handler = handler.lock().await;
        let mut recorder = Command::new("parec")
            .args(&[
                "--format=float32le",
                "--rate=48000",
                "--channels=2",
                "-d",
                "audst.monitor",
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
        recorder.stdout.as_mut().map(|stdout| {
            let mut buffer = vec![0; 48 * TIMEOUT * 2 * 4];
            let _ = stdout.read_exact(&mut buffer);
        });
        let source = Input::new(
            true,
            recorder.into(),
            Codec::FloatPcm,
            Container::Raw,
            Default::default(),
        );
        handler.play_source(source);
    }
}

#[tokio::main]
async fn main() {
    pa_load_once(&["module-null-sink", "sink_name=audst"]);
    tokio::task::spawn_blocking(|| {
        let mut old_sink = 0;
        loop {
            let mut sinks = vec![
                "Don't create loopback".to_owned(),
                "@DEFAULT_SINK@".to_owned(),
            ];
            sinks.extend(pa_get_sinks());
            let new_sink = Select::new()
                .with_prompt("Output device")
                .items(&sinks)
                .default(0)
                .clear(true)
                .interact()
                .unwrap();
            if old_sink != 0 {
                pa_unload_module(&[
                    "module-loopback",
                    "source=audst.monitor",
                    &format!("sink={}", &sinks[old_sink]),
                ]);
            }
            if new_sink != 0 {
                pa_load_once(&[
                    "module-loopback",
                    "source=audst.monitor",
                    &format!("sink={}", &sinks[new_sink]),
                ]);
            }
            old_sink = new_sink;
        }
    });
    let token = env!("DISCORD_TOKEN");
    let framework = StandardFramework::new();
    let mut client = Client::builder(&token)
        .event_handler(Handler)
        .framework(framework)
        .register_songbird()
        .await
        .expect("Failed to create client");
    let _ = client
        .start()
        .await
        .map_err(|err| println!("error: {:?}", err));
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
