//! Independent diagnostics lane: serialization and network waits never run on media workers.
use crate::{
    protocol::{MediaLane, MediaStreamHello, VERSION, receive_json, send_json},
    telemetry::{DiagnosticBatch, Telemetry},
};
use std::{sync::Arc, time::Duration};

pub async fn send_batches(
    connection: quinn::Connection,
    screens: impl Fn() -> Vec<(u32, u64, Arc<Telemetry>)>,
) {
    let Ok(mut stream) = connection.open_uni().await else {
        return;
    };
    if send_json(
        &mut stream,
        &MediaStreamHello {
            version: VERSION,
            screen_id: 0,
            generation: 0,
            lane: MediaLane::Diagnostics,
        },
    )
    .await
    .is_err()
    {
        return;
    }
    let mut timer = tokio::time::interval(Duration::from_millis(500));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { _=connection.closed()=>break, _=timer.tick()=>{} }
        for (id, generation, telemetry) in screens() {
            let batch = telemetry.drain(id, generation);
            if batch.events.is_empty() && batch.stats.is_none() {
                continue;
            }
            // Each event is size bounded; split to stay below control framing limit.
            for (index, chunk) in batch.events.chunks(24).enumerate() {
                let packet = DiagnosticBatch {
                    screen_id: id,
                    generation,
                    events: chunk.to_vec(),
                    stats: None,
                    lost_events: batch.lost_events,
                };
                if tokio::time::timeout(Duration::from_secs(2), send_json(&mut stream, &packet))
                    .await
                    .map_or(true, |r| r.is_err())
                {
                    telemetry.delivery_failed(
                        (batch.events.len() - index * 24) as u64,
                        "Diagnostics transmission stopped; video continues",
                    );
                    return;
                }
            }
            if let Some(stats) = batch.stats {
                let packet = DiagnosticBatch {
                    screen_id: id,
                    generation,
                    events: vec![],
                    stats: Some(stats),
                    lost_events: batch.lost_events,
                };
                if tokio::time::timeout(Duration::from_secs(2), send_json(&mut stream, &packet))
                    .await
                    .map_or(true, |r| r.is_err())
                {
                    telemetry.delivery_failed(
                        1,
                        "Diagnostics statistics transmission stopped; video continues",
                    );
                    return;
                }
            }
        }
    }
}
pub async fn receive_batches(
    mut stream: quinn::RecvStream,
    ingest: impl Fn(DiagnosticBatch),
) -> anyhow::Result<()> {
    loop {
        let batch = receive_json::<_, DiagnosticBatch>(&mut stream).await?;
        if batch.events.len() > 24
            || batch.events.iter().any(|e| {
                e.sample.reason.len() > 640
                    || e.capture_us > i64::MAX as u64
                    || e.sample.duration_us.is_some_and(|d| d > 3_600_000_000)
                    || (e.sample.start_us as i128 - e.capture_us as i128).abs() > 3_600_000_000
            })
        {
            anyhow::bail!("Oversized diagnostics batch");
        }
        ingest(batch);
    }
}
