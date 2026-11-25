# dbus-crossroads 0.5.3, patched

A copy of [dbus-crossroads](https://crates.io/crates/dbus-crossroads) 0.5.3 with
one change. The root `Cargo.toml` uses it in place of the published crate.

**Change:** in `src/stdimpl.rs`, `PathPropMap` is a `BTreeMap` instead of a
`HashMap`, so `GetManagedObjects` lists objects sorted by path instead of in
random order.

**Why:** BlueZ assigns GATT handles in the order in which `GetManagedObjects`
lists the characteristics. blueterm-agent re-registers its service at the same
handle after each session and relies on getting the same layout again, because
clients cache it.

`src_old`, examples and dev-dependencies were left out. Remove this copy when a
release of dbus-crossroads contains the change.
