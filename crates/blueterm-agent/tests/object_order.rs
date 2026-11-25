//! Checks that our patched dbus-crossroads is in use, see
//! third_party/dbus-crossroads/README.md.
//!
//! BlueZ assigns GATT handles in the order in which `GetManagedObjects` lists
//! the characteristics, and the agent relies on getting the same handles when
//! it re-registers its service.

#![cfg(target_os = "linux")]

use std::{cell::RefCell, collections::HashMap};

use dbus::{
    Message, Path,
    arg::{Dict, PropMap},
};
use dbus_crossroads::Crossroads;

#[test]
fn get_managed_objects_lists_objects_in_order() {
    let paths: Vec<String> = (0..8).map(|i| format!("/app/service0/char{i}")).collect();
    let mut cr = Crossroads::new();
    let om = cr.object_manager();
    cr.insert("/app", &[om], ());
    for p in &paths {
        cr.insert(p.clone(), &[], ());
    }

    let mut msg =
        Message::new_method_call("org.example", "/app", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects")
            .unwrap();
    msg.set_serial(1);
    let sent = RefCell::new(Vec::new());
    cr.handle_message(msg, &sent).unwrap();

    let sent = sent.into_inner();
    let objects: Dict<Path, HashMap<String, PropMap>, _> = sent[0].read1().unwrap();
    let listed: Vec<String> = objects.map(|(path, _)| path.to_string()).collect();
    assert_eq!(listed, paths);
}
