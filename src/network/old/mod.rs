//! Very, very WIP
//! "Delayed lockstep" networking implementation - first step towards GGPO

use std::sync::mpsc;

use macroquad::experimental::scene::{self, Handle, Node, NodeWith, RefMut};

use crate::{capabilities::NetworkReplicate, collect_input, GameInput, GameInputScheme, Player};

use nanoserde::{DeBin, SerBin};

mod connection;

pub use connection::{NetworkConnection, NetworkConnectionKind, NetworkConnectionStatus};

#[derive(Debug, DeBin, SerBin)]
pub enum NetworkMessage {
    /// Empty message, used for connection test
    Idle,
    RelayRequestId,
    RelayIdAssigned(u64),
    RelayConnectTo(u64),
    RelayConnected,
    Input {
        // current simulation frame
        frame: u64,
        input: GameInput,
    },
}

pub struct NetworkGame {
    input_scheme: GameInputScheme,

    player1: Handle<Player>,
    player2: Handle<Player>,

    frame: u64,

    tx: mpsc::Sender<NetworkMessage>,
    rx: mpsc::Receiver<NetworkMessage>,

    self_id: usize,
    // all the inputs from the beginning of the game
    // will optimize memory later
    frames_buffer: Vec<[Option<GameInput>; 2]>,
}

// // get a bitmask of received remote inputs out of frames_buffer
// fn remote_inputs_ack(remote_player_id: usize, buffer: &[[Option<Input>; 2]]) -> u8 {
//     let mut ack = 0;

//     #[allow(clippy::needless_range_loop)]
//     for i in 0..Network::CONSTANT_DELAY as usize {
//         if buffer[i][remote_player_id].is_some() {
//             ack |= 1 << i;
//         }
//     }
//     ack
// }

impl NetworkGame {
    /// 8-bit bitmask is used for ACK, to make CONSTANT_DELAY more than 8
    /// bitmask type should be changed
    const CONSTANT_DELAY: usize = 8;

    pub fn new(
        id: usize,
        socket: std::net::UdpSocket,
        input_scheme: GameInputScheme,
        player1: Handle<Player>,
        player2: Handle<Player>,
    ) -> NetworkGame {
        socket.set_nonblocking(true).unwrap();

        let (tx, rx) = mpsc::channel::<NetworkMessage>();

        let (tx1, rx1) = mpsc::channel::<NetworkMessage>();

        {
            let socket = socket.try_clone().unwrap();
            std::thread::spawn(move || {
                let socket = socket;
                loop {
                    let mut data = [0; 256];
                    match socket.recv_from(&mut data) {
                        Err(..) => {} //println!("waiting for other player"),
                        Ok((count, _)) => {
                            assert!(count < 256);
                            let message = DeBin::deserialize_bin(&data[0..count]).unwrap();

                            tx1.send(message).unwrap();
                        }
                    }
                }
            });
        }

        std::thread::spawn(move || {
            loop {
                if let Ok(message) = rx.recv() {
                    let data = SerBin::serialize_bin(&message);

                    let socket = socket.try_clone().unwrap();

                    // std::thread::spawn(move || {
                    //     std::thread::sleep(std::time::Duration::from_millis(
                    //         macroquad::rand::gen_range(0, 150),
                    //     ));
                    //     if macroquad::rand::gen_range(0, 100) > 20 {
                    //         let _ = socket.send(&data);
                    //     }
                    // });
                    socket.send(&data).unwrap();
                }
            }
        });

        let mut frames_buffer = vec![];

        // Fill first CONSTANT_DELAY frames
        // this will not really change anything - the fish will just always spend
        // first CONSTANT_DELAY frames doing nothing, not a big deal
        // But with pre-filled buffer we can avoid any special-case logic
        // at the start of the game and later on will just wait for remote
        // fish to fill up their part of the buffer
        #[allow(clippy::needless_range_loop)]
        for _ in 0..Self::CONSTANT_DELAY {
            let mut frame = [None; 2];
            frame[id as usize] = Some(GameInput::default());

            frames_buffer.push(frame);
        }

        NetworkGame {
            self_id: id,
            input_scheme,
            player1,
            player2,
            frame: Self::CONSTANT_DELAY as u64,
            tx,
            rx: rx1,
            frames_buffer,
        }
    }
}

impl Node for NetworkGame {
    fn fixed_update(mut node: RefMut<Self>) {
        let node = &mut *node;

        let own_input = collect_input(node.input_scheme);

        // Right now there are only two players, so it is possible to find out
        // remote fish id as "not ours" id. With more fish it will be more complicated
        // and ID will be part of a protocol
        let remote_id = if node.self_id == 1 { 0 } else { 1 };

        // re-send frames missing on remote fish
        // very excessive send, we should check ACK and
        // send only frames actually missing on the remote fish
        for i in
            (node.frame as i64 - Self::CONSTANT_DELAY as i64 * 2).max(0) as u64 as u64..node.frame
        {
            node.tx
                .send(NetworkMessage::Input {
                    frame: i,
                    input: node.frames_buffer[i as usize][node.self_id].unwrap(),
                })
                .unwrap();
        }

        // we just received only CONSTANT_DELAY frames, assuming we certainly
        // had remote input for all the previous frames
        // lets double check this assumption
        if node.frame > Self::CONSTANT_DELAY as _ {
            for i in 0..node.frame - Self::CONSTANT_DELAY as u64 - 1 {
                assert!(node.frames_buffer[i as usize][remote_id].is_some());
            }
        }

        // Receive other fish input
        while let Ok(message) = node.rx.try_recv() {
            if let NetworkMessage::Input { frame, input } = message {
                // frame from the future, need to wait until will simulate
                // the game enough to use this data
                if frame < node.frames_buffer.len() as _ {
                    node.frames_buffer[frame as usize][remote_id] = Some(input);
                }
            }
        }

        // // notify the other fish on the state of our input buffer
        // node.tx
        //     .send(Message::Ack {
        //         frame: node.frame,
        //         ack: remote_inputs_ack(remote_id, &node.frames_buffer),
        //     })
        //     .unwrap();

        // we have an input for "-CONSTANT_DELAY" frame, so we can
        // advance the simulation
        if let [Some(p1_input), Some(p2_input)] =
            node.frames_buffer[node.frames_buffer.len() - Self::CONSTANT_DELAY]
        {
            scene::get_node(node.player1).apply_input(p1_input);
            scene::get_node(node.player2).apply_input(p2_input);

            // advance the simulation
            for NodeWith { node, capability } in scene::find_nodes_with::<NetworkReplicate>() {
                (capability.network_update)(node);
            }

            let mut new_frame = [None, None];
            new_frame[node.self_id] = Some(own_input);

            node.frames_buffer.push(new_frame);

            node.frame += 1;
        }
    }
}
