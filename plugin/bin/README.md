# `bin/` directory

The Hermes plugin manager installs the compiled Caduceus daemon binary here at `bin/caduceus` after building from the repo's `Cargo.toml`.

This directory is populated by the plugin install hook — it's empty in the source repo. After `hermes plugin install`, you'll see:

```
bin/
└── caduceus    # the compiled daemon binary
```

The cron profile (`cron/caduceus-pulse.yaml`) calls `../bin/caduceus` directly. The plugin's post-install hook runs `cargo build --release` from the repo root and copies the binary here.