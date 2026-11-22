// A development daemon: the same body as `waku-daemon`, built without the
// release-only handling so the dev watcher can replace it in place.
fn main() -> anyhow::Result<()> {
    waku_daemon::run()
}
