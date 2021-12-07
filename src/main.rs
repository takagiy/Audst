#![feature(exit_status_error)]
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
    process::{Command, Stdio},
    time::Duration,
};

const USER_ID: UserId = UserId(unwrap_ctx!(parse_u64(env!("DISCORD_USER"))));

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn guild_create(&self, ctx: Context, guild: Guild, _is_new: bool) {
        pa_create_null_sink("audst");
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
                "--channels=1",
                "-d",
                "audst.monitor",
                "-r",
            ])
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("Failed to spawn `parec`");
        tokio::time::sleep(Duration::from_millis(50)).await;
        match recorder.try_wait() {
            Ok(Some(status)) => panic!("`parec` exited with status {}", status),
            Ok(None) => (),
            Err(e) => panic!("Failed to check `parec`'s status: {}", e),
        };
        let source = Input::new(
            false,
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

fn pa_create_null_sink(name: &str) {
    let out = Command::new("pactl")
        .args(&["list", "short", "modules"])
        .output()
        .expect("Failed to get modules");
    out.status.exit_ok().expect("Failed to get modules");
    let out = String::from_utf8(out.stdout).expect("Invalid UTF-8");
    let sink_exists = out.lines().find(|line| line.contains(name)).is_some();
    if sink_exists {
        return;
    }
    Command::new("pactl")
        .args(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={}", name),
        ])
        .status()
        .expect("Failed to create null sink")
        .exit_ok()
        .expect("Failed to create null sink");
}
