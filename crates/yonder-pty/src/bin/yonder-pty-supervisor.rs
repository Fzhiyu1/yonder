//! Standalone supervisor binary (tests and debugging). The yonder daemon embeds the same
//! entry point behind a hidden subcommand.
fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: yonder-pty-supervisor <args>"))?;
    let args = yonder_pty::SupervisorArgs::decode(&arg)?;
    yonder_pty::supervisor_main(args)
}
