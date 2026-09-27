//! Two AX.25 stations over a simulated lossy channel, on a virtual clock.

use std::time::{Duration, Instant};
use term73::ax25::{parse, Config, State, Station};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % 10_000) as f64 / 10_000.0
    }
}

fn cfg() -> Config {
    Config { t1: Duration::from_millis(300), n2: 60, ack_delay: Duration::from_millis(40), ..Config::default() }
}

/// Deliver each station's outbox to the other, dropping frames with probability `loss`.
/// Returns the number of frames each side put on the air.
fn step(a: &mut Station, b: &mut Station, now: Instant, rng: &mut Rng, loss: f64, sent: &mut (usize, usize)) {
    a.poll(now);
    b.poll(now);
    for raw in a.take_outbox() {
        sent.0 += 1;
        if rng.next() >= loss {
            b.on_frame(&parse(&raw).unwrap(), now);
        }
    }
    for raw in b.take_outbox() {
        sent.1 += 1;
        if rng.next() >= loss {
            a.on_frame(&parse(&raw).unwrap(), now);
        }
    }
}

fn run_until(a: &mut Station, b: &mut Station, clock: &mut Instant, rng: &mut Rng, loss: f64,
             sent: &mut (usize, usize), mut done: impl FnMut(&mut Station, &mut Station) -> bool) {
    for _ in 0..200_000 {
        if done(a, b) {
            return;
        }
        step(a, b, *clock, rng, loss, sent);
        *clock += Duration::from_millis(20);
    }
    panic!("condition not reached");
}

fn transfer(loss: f64, seed: u64) -> (usize, usize) {
    let mut clock = Instant::now();
    let mut rng = Rng(seed);
    let mut sent = (0, 0);
    let mut a = Station::new("N0CALL", cfg());
    let mut b = Station::new("N0GW-10", cfg());
    b.listen = true;
    a.connect("N0GW-10", &[], None, clock);
    run_until(&mut a, &mut b, &mut clock, &mut rng, loss, &mut sent,
              |a, b| a.state == State::Connected && b.state == State::Connected);
    let up: Vec<u8> = (0..5000u32).map(|i| (i * 7 + 3) as u8).collect();
    let down: Vec<u8> = (0..2000u32).map(|i| (i * 13 + 1) as u8).collect();
    a.send(&up);
    b.send(&down);
    let (mut got_a, mut got_b) = (Vec::new(), Vec::new());
    run_until(&mut a, &mut b, &mut clock, &mut rng, loss, &mut sent, |a, b| {
        got_b.extend(b.recv());
        got_a.extend(a.recv());
        got_b.len() >= up.len() && got_a.len() >= down.len() && a.all_sent() && b.all_sent()
    });
    assert_eq!(got_b, up);
    assert_eq!(got_a, down);
    a.disconnect(clock);
    run_until(&mut a, &mut b, &mut clock, &mut rng, loss, &mut sent,
              |a, b| a.state == State::Disconnected && b.state == State::Disconnected);
    sent
}

#[test]
fn clean_link() {
    let sent = transfer(0.0, 1);
    // 5000 bytes / 128 = 40 I-frames + SABM + DISC, plus a few RRs from A
    assert!(sent.0 >= 42 && sent.0 < 60, "{sent:?}");
}

#[test]
fn lossy_links() {
    for (loss, seed) in [(0.25, 7), (0.40, 11)] {
        transfer(loss, seed);
    }
}

#[test]
fn connect_attempts_limit() {
    let mut clock = Instant::now();
    let mut a = Station::new("N0CALL", cfg());
    a.connect("NOBODY-1", &[], Some(3), clock);
    let mut sabms = a.take_outbox().len();
    while a.state != State::Disconnected {
        clock += Duration::from_millis(20);
        a.poll(clock);
        sabms += a.take_outbox().len();
    }
    assert_eq!(sabms, 3);
    assert!(a.take_events().iter().any(|e| e.contains("after 3 attempts")));
}

#[test]
fn refused_connection() {
    let mut clock = Instant::now();
    let mut a = Station::new("N0CALL", cfg());
    let mut b = Station::new("N0BBS-3", cfg());          // not listening: answers SABM with DM? no, stays silent
    b.listen = false;
    b.remote = "SOMEONE".into();                          // busy with someone else -> DM to us
    a.connect("N0BBS-3", &[], Some(3), clock);
    let mut rng = Rng(3);
    let mut sent = (0, 0);
    run_until(&mut a, &mut b, &mut clock, &mut rng, 0.0, &mut sent, |a, _| a.state == State::Disconnected);
    assert!(a.take_events().iter().any(|e| e.contains("refused")));
}
