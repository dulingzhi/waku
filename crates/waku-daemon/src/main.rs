use std::io::Write as _;

fn main() -> anyhow::Result<()> {
    match waku_daemon::run() {
        Ok(()) => Ok(()),
        Err(error) => {
            // The supervisor reads stderr to explain a daemon that never became
            // ready, so the failure has to reach that stream even though
            // `main`'s `Err` would otherwise print it.
            let _ = writeln!(std::io::stderr(), "{error:#}");
            Err(error)
        }
    }
}
