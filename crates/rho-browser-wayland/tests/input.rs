//! Protocol regression fixture: no Chromium or graphics device required.
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

use rho_browser_wayland::{
    BrowserCompositor, BrowserEvent, BrowserRenderConfig, BrowserSession, TextInputState,
    TextInputUpdate, TouchInput,
};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface, wl_touch,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Debug, PartialEq)]
enum Contact {
    Down(i32, f64, f64),
    Motion(i32, f64, f64),
    Up(i32),
    Frame,
    Cancel,
}
struct Client {
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    frame: File,
    contacts: Vec<Contact>,
    text: Vec<zwp_text_input_v3::Event>,
}
impl Client {
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        self.frame.seek(SeekFrom::Start(0)).unwrap();
        self.frame.write_all(&vec![0xff; 100 * 80 * 4]).unwrap();
        let pool = self
            .shm
            .create_pool(self.frame.as_fd(), 100 * 80 * 4, qh, ());
        let buffer = pool.create_buffer(0, 100, 80, 400, wl_shm::Format::Xrgb8888, qh, ());
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage_buffer(0, 0, 100, 80);
        self.surface.commit();
        pool.destroy();
    }
}
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
    fn event(
        _: &mut Self,
        shell: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            shell.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(
        this: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            this.draw(qh);
        }
    }
}
impl Dispatch<wl_buffer::WlBuffer, ()> for Client {
    fn event(
        _: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
        }
    }
}
impl Dispatch<wl_touch::WlTouch, ()> for Client {
    fn event(
        this: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        this.contacts.push(match event {
            wl_touch::Event::Down { id, x, y, .. } => Contact::Down(id, x, y),
            wl_touch::Event::Motion { id, x, y, .. } => Contact::Motion(id, x, y),
            wl_touch::Event::Up { id, .. } => Contact::Up(id),
            wl_touch::Event::Frame => Contact::Frame,
            wl_touch::Event::Cancel => Contact::Cancel,
            _ => return,
        });
    }
}
impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for Client {
    fn event(
        this: &mut Self,
        _: &zwp_text_input_v3::ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        this.text.push(event);
    }
}
delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_shm::WlShm);
delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore wl_seat::WlSeat);
delegate_noop!(Client: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(Client: ignore zwp_text_input_manager_v3::ZwpTextInputManagerV3);

fn text_state(session: &BrowserSession<u8>, enabled: bool) -> TextInputState {
    futures_lite::future::block_on(async {
        loop {
            if let BrowserEvent::TextInput(state) = session.events().recv().await.unwrap()
                && state.enabled == enabled
            {
                return state;
            }
        }
    })
}
fn flush(session: &BrowserSession<u8>, generation: u64) {
    futures_lite::future::block_on(session.input_barrier(generation).unwrap()).unwrap();
    session.unfreeze_input(generation);
}

#[test]
fn native_contacts_and_text_batches_obey_focus_generation_and_freeze() {
    // This is the sole compositor fixture; provide a private runtime directory
    // on headless test runners that have no login session.
    let runtime = tempfile::tempdir().unwrap();
    let installed_runtime = std::env::var_os("XDG_RUNTIME_DIR").is_none();
    if installed_runtime {
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
        }
    }
    let compositor = BrowserCompositor::<u8>::launch(BrowserRenderConfig::SoftwareShmQa).unwrap();
    let session = compositor.open(1, (100, 80)).unwrap();
    let path = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
        .join(compositor.socket_name());
    let connection = Connection::from_socket(UnixStream::connect(path).unwrap()).unwrap();
    let (globals, mut queue) = registry_queue_init::<Client>(&connection).unwrap();
    let qh = queue.handle();
    let wl: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm = globals.bind(&qh, 1..=2, ()).unwrap();
    let shell: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ()).unwrap();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).unwrap();
    let _touch = seat.get_touch(&qh, ());
    let manager: zwp_text_input_manager_v3::ZwpTextInputManagerV3 =
        globals.bind(&qh, 1..=1, ()).unwrap();
    let text = manager.get_text_input(&seat, &qh, ());
    let surface = wl.create_surface(&qh, ());
    let xdg = shell.get_xdg_surface(&surface, &qh, ());
    let _toplevel = xdg.get_toplevel(&qh, ());
    surface.commit();
    let mut client = Client {
        shm,
        surface,
        frame: tempfile::tempfile().unwrap(),
        contacts: vec![],
        text: vec![],
    };
    queue.roundtrip(&mut client).unwrap();
    queue.roundtrip(&mut client).unwrap();
    let scene = futures_lite::future::block_on(async {
        loop {
            if let BrowserEvent::Scene(scene) = session.events().recv().await.unwrap() {
                return scene.id;
            }
        }
    });

    // Full-width contact IDs must not be truncated to the Wayland i32 slot.
    session.touch(TouchInput::Down {
        id: 7,
        time: 1,
        scene: scene + 999,
        x: 11.,
        y: 17.,
    });
    session.touch(TouchInput::Down {
        id: 7,
        time: 2,
        scene,
        x: 11.,
        y: 17.,
    });
    session.touch(TouchInput::Down {
        id: (1 << 32) + 7,
        time: 3,
        scene,
        x: 39.,
        y: 23.,
    });
    session.touch(TouchInput::Motion {
        id: 7,
        time: 4,
        x: 121.,
        y: -9.,
    });
    session.touch(TouchInput::Up {
        id: (1 << 32) + 7,
        time: 5,
    });
    // The ordered barrier cancels the remaining contact; frozen contacts
    // cannot leak into the new page after unfreeze.
    futures_lite::future::block_on(session.input_barrier(1).unwrap()).unwrap();
    session.touch(TouchInput::Down {
        id: 88,
        time: 6,
        scene,
        x: 3.,
        y: 5.,
    });
    flush(&session, 2);
    queue.roundtrip(&mut client).unwrap();
    assert_eq!(
        client.contacts,
        vec![
            Contact::Down(0, 11., 17.),
            Contact::Frame,
            Contact::Down(1, 39., 23.),
            Contact::Frame,
            Contact::Motion(0, 121., -9.),
            Contact::Frame,
            Contact::Up(1),
            Contact::Frame,
            Contact::Motion(0, 121., -9.),
            Contact::Cancel,
        ]
    );
    assert!(
        client
            .text
            .iter()
            .any(|event| matches!(event, zwp_text_input_v3::Event::Enter { .. }))
    );
    client.text.clear();

    text.enable();
    text.set_surrounding_text("a😀éZ".into(), 5, 7);
    text.set_cursor_rectangle(11, 17, 2, 19);
    text.commit();
    queue.roundtrip(&mut client).unwrap();
    let active = text_state(&session, true);
    assert!(
        client
            .text
            .iter()
            .any(|event| matches!(event, zwp_text_input_v3::Event::Done { serial: 1 }))
    );
    client.text.clear();
    assert_eq!(active.surrounding, Some(("a😀éZ".into(), 5, 7)));
    assert_eq!(active.cursor_rectangle, Some((11, 17, 2, 19)));
    session.text_input(TextInputUpdate {
        generation: active.generation,
        preedit: Some(("漢😀".into(), 3, 7)),
        ..Default::default()
    });
    flush(&session, 3);
    queue.roundtrip(&mut client).unwrap();
    assert!(
        matches!(&client.text[0],zwp_text_input_v3::Event::PreeditString{text:Some(value),cursor_begin:3,cursor_end:7} if value=="漢😀")
    );
    assert!(matches!(
        client.text[1],
        zwp_text_input_v3::Event::Done { serial: 1 }
    ));
    // Barrier cleanup clears composition in a separate atomic batch.
    assert!(
        matches!(&client.text[2],zwp_text_input_v3::Event::PreeditString{text:Some(value),..} if value.is_empty())
    );
    client.text.clear();

    session.text_input(TextInputUpdate {
        generation: active.generation,
        delete: Some((4, 2)),
        commit: Some("x".into()),
        preedit: Some((String::new(), 0, 0)),
    });
    flush(&session, 4);
    queue.roundtrip(&mut client).unwrap();
    assert!(matches!(
        client.text[0],
        zwp_text_input_v3::Event::DeleteSurroundingText {
            before_length: 4,
            after_length: 2
        }
    ));
    assert!(
        matches!(&client.text[1],zwp_text_input_v3::Event::CommitString{text:Some(value)} if value=="x")
    );
    assert_eq!(
        client
            .text
            .iter()
            .filter(|event| matches!(event, zwp_text_input_v3::Event::CommitString { .. }))
            .count(),
        1
    );
    client.text.clear();

    // A client can publish another surrounding snapshot while composing.
    // Acknowledging that commit must retain, not accidentally cancel, preedit.
    session.text_input(TextInputUpdate {
        generation: active.generation,
        preedit: Some(("é😀".into(), -1, -1)),
        ..Default::default()
    });
    for _ in 0..100 {
        queue.roundtrip(&mut client).unwrap();
        if client.text.iter().any(|event| {
            matches!(event,
            zwp_text_input_v3::Event::PreeditString { text: Some(value), .. } if value=="é😀")
        }) {
            break;
        }
    }
    assert!(client.text.iter().any(|event| matches!(event,
        zwp_text_input_v3::Event::PreeditString { text: Some(value), .. } if value=="é😀")));
    client.text.clear();
    text.set_surrounding_text("a😀éZ".into(), 7, 5);
    text.commit();
    queue.roundtrip(&mut client).unwrap();
    assert!(
        matches!(&client.text[0],zwp_text_input_v3::Event::PreeditString {
        text:Some(value), cursor_begin:-1, cursor_end:-1
    } if value=="é😀")
    );
    assert!(matches!(
        client.text[1],
        zwp_text_input_v3::Event::Done { serial: 2 }
    ));
    client.text.clear();

    text.disable();
    text.commit();
    queue.roundtrip(&mut client).unwrap();
    let disabled = text_state(&session, false);
    assert_ne!(disabled.generation, active.generation);
    assert!(
        client
            .text
            .iter()
            .any(|event| matches!(event, zwp_text_input_v3::Event::Done { serial: 3 }))
    );
    client.text.clear();
    session.text_input(TextInputUpdate {
        generation: active.generation,
        commit: Some("stale".into()),
        ..Default::default()
    });
    flush(&session, 5);
    queue.roundtrip(&mut client).unwrap();
    assert!(client.text.is_empty());
    drop(compositor);
    if installed_runtime {
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
    }
}
