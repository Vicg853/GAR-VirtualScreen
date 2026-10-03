use bridge_core::protocol::{
    ClientMessage, HostMessage, MediaHeader, MediaKind, receive_json, receive_media_or_end,
    send_control, send_media,
};
use bridge_core::{
    config::{Acceleration, Codec, CodecOption, negotiate},
    security::{authenticate_host, authenticate_viewer, client_endpoint, server_endpoint},
};
use std::net::SocketAddr;

async fn connection_pair() -> (
    quinn::Endpoint,
    quinn::Endpoint,
    quinn::Connection,
    quinn::Connection,
) {
    let host = server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let client = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let outgoing = client
        .connect(host.local_addr().unwrap(), "airbridge.local")
        .unwrap();
    let incoming = async { host.accept().await.unwrap().await.unwrap() };
    let (viewer, sender) = tokio::join!(outgoing, incoming);
    (host, client, sender, viewer.unwrap())
}

#[tokio::test]
async fn stopping_one_video_lane_keeps_control_and_other_screens_alive() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (_host, _client, sender, viewer) = connection_pair().await;
        let (mut controls, responses) = viewer.open_bi().await.unwrap();
        send_control(&mut controls, &ClientMessage::Stop)
            .await
            .unwrap();
        let (mut replies, mut requests) = sender.accept_bi().await.unwrap();
        assert!(matches!(
            receive_json::<_, ClientMessage>(&mut requests)
                .await
                .unwrap(),
            ClientMessage::Stop
        ));
        let mut stalled = sender.open_uni().await.unwrap();
        stalled.write_all(&[0]).await.unwrap(); // An incomplete packet must not block other channels.
        let mut stalled_reader = viewer.accept_uni().await.unwrap();
        let mut video = sender.open_uni().await.unwrap();
        let header = MediaHeader {
            screen_id: 2,
            generation: 1,
            kind: MediaKind::Video,
            sequence: 1,
            timestamp_us: 0,
            keyframe: true,
            width: 1920,
            height: 1080,
            sample_rate: 0,
            channels: 0,
        };
        send_media(&mut video, &header, b"frame").await.unwrap();
        let mut video_reader = viewer.accept_uni().await.unwrap();
        assert_eq!(
            receive_media_or_end(&mut video_reader)
                .await
                .unwrap()
                .unwrap()
                .1,
            b"frame"
        );
        stalled_reader.stop(7u32.into()).unwrap();
        assert_eq!(stalled.stopped().await.unwrap(), Some(7u32.into()));
        send_control(
            &mut replies,
            &HostMessage::ScreenRemoved {
                request_id: Some(8),
                screen_id: 1,
            },
        )
        .await
        .unwrap();
        let mut responses = responses;
        assert!(matches!(
            receive_json::<_, HostMessage>(&mut responses)
                .await
                .unwrap(),
            HostMessage::ScreenRemoved { screen_id: 1, .. }
        ));
        send_media(&mut video, &header, b"next").await.unwrap();
        video.finish().unwrap();
        assert_eq!(
            receive_media_or_end(&mut video_reader)
                .await
                .unwrap()
                .unwrap()
                .1,
            b"next"
        );
        assert!(
            receive_media_or_end(&mut video_reader)
                .await
                .unwrap()
                .is_none()
        );
        viewer.close(0u32.into(), b"done");
        sender.closed().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn connection_close_propagates_in_both_directions() {
    for host_closes in [true, false] {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (_host, _client, sender, viewer) = connection_pair().await;
            if host_closes {
                sender.close(0u32.into(), b"host quit");
                viewer.closed().await;
            } else {
                viewer.close(0u32.into(), b"viewer quit");
                sender.closed().await;
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn incorrect_pin_rejects_session() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (_host, _client, sender, viewer) = connection_pair().await;
        let (host_result, viewer_result) = tokio::join!(
            authenticate_host(&sender, "123456"),
            authenticate_viewer(&viewer, "654321")
        );
        assert!(host_result.is_err());
        assert!(viewer_result.is_err());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn pin_authentication_and_codec_selection() {
    let server = server_endpoint("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let address = server.local_addr().unwrap();
    let (authenticated, confirmed) = tokio::sync::oneshot::channel();
    let host_task = tokio::spawn(async move {
        let connection = server.accept().await.unwrap().await.unwrap();
        authenticate_host(&connection, "123456").await.unwrap();
        authenticated.send(()).unwrap();
        connection.closed().await;
    });
    let client = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let connection = client
        .connect(address, "airbridge.local")
        .unwrap()
        .await
        .unwrap();
    authenticate_viewer(&connection, "123456").await.unwrap();
    confirmed.await.unwrap();
    connection.close(0u32.into(), b"done");
    host_task.await.unwrap();

    let options = [
        CodecOption {
            codec: Codec::Av1,
            acceleration: Acceleration::Software,
        },
        CodecOption {
            codec: Codec::H264,
            acceleration: Acceleration::Hardware,
        },
    ];
    assert_eq!(negotiate(&options, &options, None).unwrap(), options[1]);
}
