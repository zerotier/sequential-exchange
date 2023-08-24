use std::sync::Arc;

use tokio::{sync::mpsc, task};

use seq_ex::{
    tokio::{MpscTransport, ReplyGuard, SeqExTokio},
    Packet,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Payload {
    Hello,
    Space,
    World,
    Exclamation,
    NewLine,
}
use Payload::*;

async fn receive(reply_guard: ReplyGuard<'_, &MpscTransport<Payload>, Payload, Payload>, payload: Payload) -> Option<()> {
    match payload {
        Hello => {
            print!("Hello");

            let (reply_guard, payload) = reply_guard.reply(false, Space).await.ok()?;
            if payload != World {
                return None;
            }
            print!("World");

            let (reply_guard, payload) = reply_guard.reply(false, Exclamation).await.ok()?;
            if payload != NewLine {
                return None;
            }
            print!("\n");
            drop(reply_guard);
        }
        _ => {
            assert!(false, "Unsolicited payload received: {:?}", payload);
        }
    }
    Some(())
}

async fn say_hello(seq: &SeqExTokio<Payload, Payload>, transport: &MpscTransport<Payload>) -> Option<()> {
    let (reply_guard, payload) = seq.send(transport, false, Hello).await.ok()?;
    if payload != Space {
        return None;
    }
    print!(" ");

    let (reply_guard, payload) = reply_guard.reply(false, World).await.ok()?;
    if payload != Exclamation {
        return None;
    }
    print!("!");

    let _ = reply_guard.reply(false, NewLine).await;
    Some(())
}

fn peer_main(transport: MpscTransport<Payload>, mut recv: mpsc::Receiver<Packet<Payload>>) -> Arc<SeqExTokio<Payload, Payload>> {
    let (seq, mut service) = SeqExTokio::new_default();
    let peer = Arc::new(seq);
    let peer_weak = Arc::downgrade(&peer);
    let tl = transport.clone();
    task::spawn(async move {
        while let Some(peer) = peer_weak.upgrade() {
            peer.service_task(&tl, &mut service).await;
        }
    });
    let peer_weak = Arc::downgrade(&peer);
    task::spawn(async move {
        while let Some(packet) = recv.recv().await {
            if let Some(peer) = peer_weak.upgrade() {
                let tl = transport.clone();
                task::spawn(async move {
                    if let Ok((g, payload)) = peer.receive(&tl, packet).await {
                        receive(g, payload).await;
                    }
                });
            } else {
                break;
            }
        }
    });
    peer
}

#[tokio::main]
async fn main() {
    let (transport2, recv1) = MpscTransport::new(32);
    let (transport1, recv2) = MpscTransport::new(32);
    let peer1 = peer_main(transport1.clone(), recv1);
    let _peer2 = peer_main(transport2, recv2);

    say_hello(&peer1, &transport1).await;
}
#[test]
fn test() {
    main()
}
