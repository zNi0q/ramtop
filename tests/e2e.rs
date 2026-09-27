//! End-to-end tests: spawn real processes that hold RAM, verify ramtop sees
//! them, and kill them both through the library and by clicking through the
//! real desktop UI (egui_kittest drives it via the accessibility tree).

use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use eframe::egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use ramtop::gui::{RamApp, apply_style};
use ramtop::{filter, kill, snapshot};
use sysinfo::System;

const HOG_MB: u64 = 64;

/// Spawn a python process holding HOG_MB of touched memory, tagged with `marker`.
fn spawn_hog(marker: &str, ignore_term: bool) -> Child {
    let code = format!(
        "import signal,time\n{}\nb = b'x' * ({HOG_MB} * 1024 * 1024)\ntime.sleep(120)",
        if ignore_term {
            "signal.signal(signal.SIGTERM, signal.SIG_IGN)"
        } else {
            ""
        }
    );
    Command::new("python3")
        .args(["-c", &code, marker])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn python3")
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    false
}

fn exited(child: &mut Child) -> bool {
    child.try_wait().unwrap().is_some()
}

/// Wait until the hog has finished allocating.
fn wait_allocated(child: &Child) {
    let mut sys = System::new();
    let ok = wait_until(Duration::from_secs(10), || {
        snapshot(&mut sys)
            .iter()
            .any(|p| p.pid == child.id() && p.mem >= HOG_MB * 1024 * 1024)
    });
    assert!(ok, "hog never reached {HOG_MB} MiB");
}

#[test]
fn snapshot_sees_hog_and_kill_terminates_it() {
    let marker = format!("ramtop-lib-{}", std::process::id());
    let mut hog = spawn_hog(&marker, false);
    wait_allocated(&hog);

    let mut sys = System::new();
    let procs = snapshot(&mut sys);
    assert!(
        procs.windows(2).all(|w| w[0].mem >= w[1].mem),
        "not sorted by RAM"
    );
    let found = filter(&procs, &marker);
    assert_eq!(found.len(), 1, "filter by command-line marker");
    let p = found[0];
    assert_eq!(p.pid, hog.id());
    assert!(p.name.starts_with("python"), "name was {}", p.name);
    assert_ne!(p.unit, "", "unit column should never be empty");

    kill(hog.id(), false).expect("SIGTERM");
    assert!(
        wait_until(Duration::from_secs(5), || exited(&mut hog)),
        "hog survived SIGTERM"
    );

    let err = kill(hog.id(), false).unwrap_err();
    assert!(err.contains("no longer exists"), "got: {err}");
}

#[test]
fn kill_reports_permission_denied() {
    // Only meaningful (and only safe) when not root.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let err = kill(1, false).unwrap_err();
    assert!(err.contains("permission denied"), "got: {err}");
}

fn gui() -> Harness<'static, RamApp> {
    Harness::builder()
        .with_size([1600.0, 1000.0])
        .build_eframe(|cc| {
            apply_style(&cc.egui_ctx);
            RamApp::default()
        })
}

/// Run a few frames so clicks and typed text are processed.
fn settle(h: &mut Harness<'_, RamApp>) {
    h.run_steps(4);
}

#[test]
fn gui_shows_every_view() {
    let mut h = gui();
    settle(&mut h);
    for label in [
        "RAM used",
        "Swap used",
        "Biggest process",
        "RAM usage over time",
        "Top 10 processes",
        "Where the RAM goes",
    ] {
        h.get_by_label(label);
    }
    for (nav, heading) in [
        ("Highest consumers", "Highest RAM consumers"),
        (
            "Lowest consumers",
            "Lowest RAM consumers (excluding kernel threads)",
        ),
        ("Apps (", "Apps — RAM per app"),
        ("Background (", "Background services — RAM per service"),
        ("All processes (", "All processes"),
    ] {
        h.get_by_label_contains(nav).click();
        settle(&mut h);
        h.get_by_label(heading);
        assert!(
            !h.get_all_by_label("Terminate")
                .collect::<Vec<_>>()
                .is_empty(),
            "{nav}: no rows"
        );
    }
}

#[test]
fn gui_filter_cancel_terminate_and_force_kill() {
    let marker = format!("ramtop-gui-{}", std::process::id());
    let mut hog = spawn_hog(&marker, true); // ignores SIGTERM
    wait_allocated(&hog);
    let pid = hog.id().to_string();

    let mut h = gui();
    settle(&mut h);
    h.get_by_label_contains("All processes (").click();
    settle(&mut h);

    // Filter down to the hog.
    let input = h.get_by_role(Role::TextInput);
    input.focus();
    input.type_text(&marker);
    settle(&mut h);
    h.get_by_label("1 shown");
    h.get_by_label(&pid);
    h.get_by_label("python3");
    let ram = h
        .get_by_label_contains(" MiB")
        .accesskit_node()
        .value()
        .unwrap();
    let shown: f64 = ram.split_whitespace().next().unwrap().parse().unwrap();
    assert!(shown >= HOG_MB as f64, "RAM shown as {ram}");

    // Cancel leaves it running.
    h.get_by_label("Terminate").click();
    settle(&mut h);
    h.get_by_label("Terminate python3?");
    h.get_by_label("Cancel").click();
    settle(&mut h);
    h.get_by_label("Cancelled");
    sleep(Duration::from_millis(300));
    assert!(!exited(&mut hog), "cancel still killed the process");

    // SIGTERM is ignored by this hog, so it survives.
    h.get_by_label("Terminate").click();
    settle(&mut h);
    h.get_by_label("Yes, terminate").click();
    settle(&mut h);
    h.get_by_label(&format!("Sent SIGTERM to python3 (PID {pid})"));
    sleep(Duration::from_millis(500));
    assert!(!exited(&mut hog), "SIG_IGN hog should survive SIGTERM");

    // Force kill works and the row disappears.
    h.get_by_label("Force kill").click();
    settle(&mut h);
    h.get_by_label("Force kill python3?");
    h.get_by_label("Yes, force kill").click();
    settle(&mut h);
    h.get_by_label(&format!("Sent SIGKILL to python3 (PID {pid})"));
    assert!(
        wait_until(Duration::from_secs(5), || exited(&mut hog)),
        "hog survived SIGKILL"
    );
    h.state_mut().refresh();
    settle(&mut h);
    assert!(
        h.query_by_label(&pid).is_none(),
        "killed process still listed"
    );
    h.get_by_label("0 shown");
}

/// Renders each view to target/screenshots/*.png for a visual check.
/// Run with: cargo test --release -- --ignored screenshots
#[test]
#[ignore]
fn screenshots() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/screenshots");
    std::fs::create_dir_all(dir).unwrap();
    let mut h = gui();
    settle(&mut h);
    // Two samples so the history chart has a line.
    h.state_mut().refresh();
    settle(&mut h);
    for (nav, file) in [
        ("Overview", "overview"),
        ("Highest consumers", "highest"),
        ("Background (", "background"),
    ] {
        h.get_by_label_contains(nav).click();
        settle(&mut h);
        h.render()
            .unwrap()
            .save(format!("{dir}/{file}.png"))
            .unwrap();
    }
}
