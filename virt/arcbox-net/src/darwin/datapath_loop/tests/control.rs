//! `DatapathControl`: checkpoint resets carry what the guest has received.

use super::*;
use arcbox_packet::ethernet::{
    SynParams, TcpFrameParams, build_tcp_ack_frame, build_tcp_syn_frame,
};
use arcbox_packet::ip::{IpPacketView, TcpPacketView};
use tokio::io::AsyncWriteExt;

fn tcp(frame: &[u8]) -> TcpPacketView<'_> {
    TcpPacketView::parse(IpPacketView::parse(&frame[14..]).unwrap()).unwrap()
}

/// A restored guest has received exactly the bytes in its RAM, so the reset
/// a checkpoint saves for a flow must sit at SND.NXT after the last byte
/// sent: one short and Linux answers with a challenge ACK, one past and it
/// drops the reset. `send_to_guest` must then deliver that frame unchanged.
#[tokio::test(flavor = "current_thread")]
async fn abort_tcp_returns_resets_at_snd_nxt_and_send_to_guest_delivers_them() {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let (datapath, guest) = fixture(&cancel);
    let control = datapath.control();
    let running = tokio::spawn(datapath.run());
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        let mut params = TcpFrameParams {
            src_ip: GUEST_IP,
            dst_ip: Ipv4Addr::LOCALHOST,
            src_port: 40500,
            dst_port: listener.local_addr().unwrap().port(),
            seq: 7001,
            ack: 0,
            window: 65535,
            src_mac: GUEST_MAC,
            dst_mac: GATEWAY_MAC,
        };
        let syn = build_tcp_syn_frame(&SynParams {
            src_ip: params.src_ip,
            dst_ip: params.dst_ip,
            src_port: params.src_port,
            dst_port: params.dst_port,
            seq: params.seq - 1,
            src_mac: GUEST_MAC,
            dst_mac: GATEWAY_MAC,
            mss: 1460,
            wscale: None,
            sack_permitted: false,
        });
        fd_write(guest.as_raw_fd(), &syn).unwrap();
        let (mut upstream, _) = listener.accept().await.unwrap();
        let mut frame = [0; 2048];
        let n = guest.recv(&mut frame).await.unwrap();
        let isn = tcp(&frame[..n]).sequence();
        params.ack = isn.wrapping_add(1);
        fd_write(guest.as_raw_fd(), &build_tcp_ack_frame(&params)).unwrap();

        let payload = b"bytes the guest received";
        upstream.write_all(payload).await.unwrap();
        let mut received = 0;
        while received < payload.len() {
            let n = guest.recv(&mut frame).await.unwrap();
            received += tcp(&frame[..n]).payload().len();
        }

        let resets = {
            let control = control.clone();
            tokio::task::spawn_blocking(move || control.abort_tcp(Duration::from_secs(1)))
                .await
                .unwrap()
                .unwrap()
        };
        assert_eq!(resets.len(), 1);
        let reset = tcp(&resets[0]);
        assert_eq!(reset.flags(), 0x04);
        assert_eq!(
            reset.sequence(),
            isn.wrapping_add(1).wrapping_add(payload.len() as u32)
        );
        assert_eq!(
            (reset.source_port(), reset.destination_port()),
            (params.dst_port, params.src_port)
        );

        control.send_to_guest(resets.clone()).unwrap();
        let n = guest.recv(&mut frame).await.unwrap();
        assert_eq!(&frame[..n], &resets[0][..]);
        // The abort released the host socket.
        let mut rest = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut upstream, &mut rest).await;
    })
    .await;
    cancel.cancel();
    running.await.unwrap().unwrap();
    result.expect("abort and redelivery must complete");
}
