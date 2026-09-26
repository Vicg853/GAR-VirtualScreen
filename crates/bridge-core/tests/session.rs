use bridge_core::{
    config::{Acceleration, Codec, CodecOption, negotiate},
    security::{authenticate_host, authenticate_viewer, client_endpoint, server_endpoint},
};
use std::net::SocketAddr;

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
