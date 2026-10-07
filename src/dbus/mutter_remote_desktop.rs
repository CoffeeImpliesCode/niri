//! `org.gnome.Mutter.RemoteDesktop` implementation. `xdg-desktop-portal-gnome` implements the
//! Remote Desktop portal on top of this.

use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
#[cfg(feature = "xdp-gnome-screencast")]
use std::sync::Arc;

use bitflags::bitflags;
use enumflags2::BitFlags;
#[cfg(feature = "xdp-gnome-screencast")]
use futures_util::lock::Mutex;
use serde::{Deserialize, Serialize};
use smithay::backend::input::{AxisSource, InputTime, KeyState, Keycode};
use smithay::utils::{Logical, Point};
use zbus::message::Header;
use zbus::names::{BusName, OwnedUniqueName};
#[cfg(feature = "xdp-gnome-screencast")]
use zbus::object_server::InterfaceRef;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{self, DeserializeDict, OwnedObjectPath, SerializeDict, Type};
use zbus::{fdo, interface, ObjectServer};

use super::Start;
use crate::input::dbus_remote_desktop_backend::{
    RdAbsolutePosition, RdEventAdapter, RdInputBackend, RdKeyboardKeyEvent, RdPointerAxisEvent,
    RdPointerButtonEvent, RdPointerMotionAbsoluteEvent, RdPointerMotionEvent, RdTouchEvent,
    UnitIntervalPointKind,
};
use crate::utils::{global_bounding_rectangle_ipc, RemoteDesktopSessionId};

#[cfg(feature = "xdp-gnome-screencast")]
pub(super) mod shared {
    use std::collections::HashMap;
    use std::sync::Arc;

    use futures_util::lock::Mutex;
    use zbus::object_server::InterfaceRef;

    use crate::utils::RemoteDesktopSessionId;

    /// Data shared between `org.gnome.Mutter.ScreenCast` and `org.gnome.Mutter.RemoteDesktop`
    #[derive(Default)]
    pub struct RemoteDesktopShared {
        pub(in super::super) sessions:
            HashMap<RemoteDesktopSessionId, InterfaceRef<super::Session>>,
    }
    impl RemoteDesktopShared {
        pub fn new_arc_mutex() -> Arc<Mutex<Self>> {
            Arc::new(Mutex::new(Self::default()))
        }
    }
}

const PORTAL_BACKEND_NAME: &str = "org.freedesktop.impl.portal.desktop.gnome";

async fn authorize_portal_backend(
    connection: &zbus::Connection,
    header: &Header<'_>,
    expected_sender: Option<&OwnedUniqueName>,
) -> fdo::Result<OwnedUniqueName> {
    let Some(sender) = header.sender() else {
        return Err(fdo::Error::Failed("D-Bus caller has no sender".to_owned()));
    };
    let sender = OwnedUniqueName::from(sender.to_owned());

    let proxy = fdo::DBusProxy::new(connection)
        .await
        .map_err(|err| fdo::Error::Failed(format!("Cannot verify portal backend: {err}")))?;
    let backend_name = BusName::try_from(PORTAL_BACKEND_NAME).unwrap();
    let owner = proxy
        .get_name_owner(backend_name)
        .await
        .map_err(|err| fdo::Error::Failed(format!("Cannot verify portal backend: {err}")))?;
    if !is_authorized_portal_sender(&sender, expected_sender, &owner) {
        return Err(fdo::Error::Failed(
            "Only the creating connection that owns the GNOME portal backend may use remote desktop"
                .to_owned(),
        ));
    }

    Ok(sender)
}
fn is_authorized_portal_sender(
    sender: &OwnedUniqueName,
    expected_sender: Option<&OwnedUniqueName>,
    current_owner: &OwnedUniqueName,
) -> bool {
    sender.as_str() == current_owner.as_str()
        && expected_sender.is_none_or(|expected| expected.as_str() == sender.as_str())
}

async fn authorize_session_request(
    session: &Session,
    connection: &zbus::Connection,
    header: &Header<'_>,
) -> fdo::Result<()> {
    authorize_portal_backend(connection, header, Some(&session.creator)).await?;
    Ok(())
}
type InputEvent = smithay::backend::input::InputEvent<RdInputBackend>;

enum HeldInput {
    Key(u32, bool),
    Button(i32, bool),
    Touch(u32, bool),
}

pub enum RemoteDesktopDBusToCalloop {
    RemoveEisHandler {
        session_id: RemoteDesktopSessionId,
    },
    NewEisContext {
        session_id: RemoteDesktopSessionId,
        ctx: reis::eis::Context,
        exposed_device_types: BitFlags<MutterXdpDeviceType>,
        setup_result: async_channel::Sender<Result<(), String>>,
    },
    EmulateInput {
        session_id: RemoteDesktopSessionId,
        event: InputEvent,
        /// Teardown release; apply after the session's active-input gate is cleared.
        release: bool,
    },
    EmulateKeysym {
        /// X11 keysym, like in the `xkeysym` crate
        keysym: u32,
        state: KeyState,
        session_id: RemoteDesktopSessionId,
        time: InputTime,
        /// Keyboard source for origin-aware input tracking.
        keyboard_source: smithay::input::keyboard::KeyboardSource,
        /// Teardown release; apply after the session's active-input gate is cleared.
        release: bool,
    },
    /// Increments the number of remote desktop sessions that need touch capability on the
    /// seat.
    IncTouchSession,
    /// Decrements the number of remote desktop sessions that need touch capability on the
    /// seat.
    DecTouchSession,
    SetSessionActive {
        session_id: RemoteDesktopSessionId,
        active: bool,
    },
}

// == MAIN INTERFACE ==

/// D-Bus object for the remote desktop portal's implementation
pub(super) struct RemoteDesktop {
    pub(super) to_calloop: calloop::channel::Sender<RemoteDesktopDBusToCalloop>,
    pub(super) session_close_receiver: async_channel::Receiver<RemoteDesktopSessionId>,
    #[cfg(feature = "xdp-gnome-screencast")]
    pub(super) shared: Arc<Mutex<shared::RemoteDesktopShared>>,
}

impl Start for RemoteDesktop {
    fn start(self, monitor: bool) -> anyhow::Result<zbus::blocking::Connection> {
        let session_close_receiver = self.session_close_receiver.clone();
        #[cfg(feature = "xdp-gnome-screencast")]
        let shared = self.shared.clone();

        let conn = zbus::blocking::Connection::session()?;

        conn.object_server()
            .at("/org/gnome/Mutter/RemoteDesktop", self)?;
        super::request_name(&conn, "org.gnome.Mutter.RemoteDesktop", monitor)?;

        #[cfg(feature = "xdp-gnome-screencast")]
        {
            let server = conn.object_server().inner().clone();
            let task = conn.inner().executor().spawn(
                async move {
                    while let Ok(session_id) = session_close_receiver.recv().await {
                        let iface = shared.lock().await.sessions.get(&session_id).cloned();
                        let Some(iface) = iface else {
                            continue;
                        };
                        let ctxt = iface.signal_emitter();
                        iface.get_mut().await.stop(&server, &ctxt).await;
                    }
                },
                "closing disconnected RemoteDesktop sessions",
            );
            task.detach();
        }

        Ok(conn)
    }
}

#[interface(
    name = "org.gnome.Mutter.RemoteDesktop",
    spawn = false,
    introspection_docs = false
)]
impl RemoteDesktop {
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let creator = authorize_portal_backend(connection, &header, None).await?;
        let session_id = RemoteDesktopSessionId::next();
        let path = format!(
            "/org/gnome/Mutter/RemoteDesktop/Session/u{}",
            session_id.get()
        );
        let path = OwnedObjectPath::try_from(path).unwrap();

        debug!("Created new RemoteDesktop.Session with ID {}", session_id);

        let session = Session {
            id: session_id,
            id_str: session_id.to_string(),
            creator,
            to_calloop: self.to_calloop.clone(),
            #[cfg(feature = "xdp-gnome-screencast")]
            shared: self.shared.clone(),
            active: false,
            using_eis: false,
            using_touch: false,
            keyboard_source: smithay::input::keyboard::KeyboardSource::new_auxiliary(),
            held_keys: HashSet::new(),
            held_keysyms: HashSet::new(),
            held_buttons: HashSet::new(),
            held_touches: HashSet::new(),
            #[cfg(feature = "xdp-gnome-screencast")]
            screen_cast_session: None,
        };

        match server.at(&path, session).await {
            Ok(true) => {
                #[cfg(feature = "xdp-gnome-screencast")]
                {
                    let iface = server.interface(&path).await.unwrap();
                    self.shared.lock().await.sessions.insert(session_id, iface);
                }
            }
            Ok(false) => return Err(fdo::Error::Failed("session path already exists".to_owned())),
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating session object: {err:?}"
                )))
            }
        }

        Ok(path)
    }

    /// Bitmask of supported device types
    #[zbus(property)]
    async fn supported_device_types(&self) -> u32 {
        BitFlags::<MutterXdpDeviceType>::all().bits()
    }

    #[zbus(property)]
    async fn version(&self) -> i32 {
        1
    }
}

// == SESSION ==

/// D-Bus object for a remote desktop session
pub(super) struct Session {
    id: RemoteDesktopSessionId,
    id_str: String,
    creator: OwnedUniqueName,
    to_calloop: calloop::channel::Sender<RemoteDesktopDBusToCalloop>,
    #[cfg(feature = "xdp-gnome-screencast")]
    shared: Arc<Mutex<shared::RemoteDesktopShared>>,
    pub active: bool,
    using_eis: bool,
    /// Whether the main thread has been informed that this requires touch capability on the
    /// seat.
    using_touch: bool,
    /// Keyboard source for origin-aware teardown.
    keyboard_source: smithay::input::keyboard::KeyboardSource,
    /// Inputs injected by this session and still held.
    held_keys: HashSet<u32>,
    held_keysyms: HashSet<u32>,
    held_buttons: HashSet<i32>,
    held_touches: HashSet<u32>,
    #[cfg(feature = "xdp-gnome-screencast")]
    pub screen_cast_session: Option<(
        InterfaceRef<super::mutter_screen_cast::Session>,
        ObjectServer,
    )>,
}

impl Session {
    pub(super) async fn authorize_linked_screencast(
        &self,
        connection: &zbus::Connection,
        header: &Header<'_>,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, header).await
    }

    fn authorize_input(&self) -> fdo::Result<()> {
        if !self.active {
            return Err(fdo::Error::Failed(
                "Remote desktop session is not active".to_owned(),
            ));
        }
        Ok(())
    }

    fn emulate_input(&mut self, event: InputEvent) {
        if self.authorize_input().is_err() {
            return;
        }
        if matches!(
            event,
            InputEvent::TouchDown { .. }
                | InputEvent::TouchMotion { .. }
                | InputEvent::TouchUp { .. }
                | InputEvent::TouchCancel { .. }
                | InputEvent::TouchFrame { .. }
        ) && !self.using_touch
        {
            if let Err(err) = self
                .to_calloop
                .send(RemoteDesktopDBusToCalloop::IncTouchSession)
            {
                warn!("error sending IncTouchSession to calloop: {err:?}");
            } else {
                self.using_touch = true;
            }
        }

        let held_input = match &event {
            InputEvent::Keyboard { event } => Some(HeldInput::Key(
                event.inner.keycode.raw(),
                event.inner.state == KeyState::Pressed,
            )),
            InputEvent::PointerButton { event } => {
                Some(HeldInput::Button(event.inner.button, event.inner.state))
            }
            InputEvent::TouchDown { event } => Some(HeldInput::Touch(event.inner.slot, true)),
            InputEvent::TouchUp { event } | InputEvent::TouchCancel { event } => {
                Some(HeldInput::Touch(event.inner.slot, false))
            }
            _ => None,
        };
        if let Err(err) = self
            .to_calloop
            .send(RemoteDesktopDBusToCalloop::EmulateInput {
                session_id: self.id,
                event,
                release: false,
            })
        {
            warn!("error sending EmulateInput to calloop: {err:?}");
            return;
        }

        match held_input {
            Some(HeldInput::Key(code, true)) => {
                self.held_keys.insert(code);
            }
            Some(HeldInput::Key(code, false)) => {
                self.held_keys.remove(&code);
            }
            Some(HeldInput::Button(button, true)) => {
                self.held_buttons.insert(button);
            }
            Some(HeldInput::Button(button, false)) => {
                self.held_buttons.remove(&button);
            }
            Some(HeldInput::Touch(slot, true)) => {
                self.held_touches.insert(slot);
            }
            Some(HeldInput::Touch(slot, false)) => {
                self.held_touches.remove(&slot);
            }
            None => (),
        }
    }
    fn emulate_touch_input(&mut self, event: InputEvent, slot: u32) {
        self.emulate_input(event);
        self.emulate_input(InputEvent::TouchFrame {
            event: self.wrap_event(RdTouchEvent { slot, extra: () }),
        });
    }

    fn send_release(&self, event: InputEvent) {
        if let Err(err) = self
            .to_calloop
            .send(RemoteDesktopDBusToCalloop::EmulateInput {
                session_id: self.id,
                event,
                release: true,
            })
        {
            warn!("error sending remote input release to calloop: {err:?}");
        }
    }

    fn release_held_input(&mut self) {
        for keysym in std::mem::take(&mut self.held_keysyms) {
            if let Err(err) = self
                .to_calloop
                .send(RemoteDesktopDBusToCalloop::EmulateKeysym {
                    keysym,
                    state: KeyState::Released,
                    session_id: self.id,
                    time: Self::time_now(),
                    keyboard_source: self.keyboard_source,
                    release: true,
                })
            {
                warn!("error sending remote keysym release to calloop: {err:?}");
            }
        }
        for keycode in std::mem::take(&mut self.held_keys) {
            self.send_release(InputEvent::Keyboard {
                event: self.wrap_event(RdKeyboardKeyEvent {
                    keycode: Keycode::new(keycode),
                    state: KeyState::Released,
                }),
            });
        }
        for button in std::mem::take(&mut self.held_buttons) {
            self.send_release(InputEvent::PointerButton {
                event: self.wrap_event(RdPointerButtonEvent {
                    button,
                    state: false,
                }),
            });
        }
        let held_touches = std::mem::take(&mut self.held_touches);
        for slot in &held_touches {
            self.send_release(InputEvent::TouchUp {
                event: self.wrap_event(RdTouchEvent {
                    slot: *slot,
                    extra: (),
                }),
            });
        }
        if !held_touches.is_empty() {
            self.send_release(InputEvent::TouchFrame {
                event: self.wrap_event(RdTouchEvent { slot: 0, extra: () }),
            });
        }
    }
    fn stop_input(&mut self) {
        if self.using_eis {
            let _ = self
                .to_calloop
                .send(RemoteDesktopDBusToCalloop::RemoveEisHandler {
                    session_id: self.id,
                });
            self.using_eis = false;
        }

        if self.using_touch {
            let _ = self
                .to_calloop
                .send(RemoteDesktopDBusToCalloop::DecTouchSession);
            self.using_touch = false;
        }
    }
    fn wrap_event<Ev>(&self, inner: Ev) -> RdEventAdapter<Ev> {
        RdEventAdapter {
            session_id: self.id,
            source: self.keyboard_source,
            time: Self::time_now(),
            inner,
        }
    }
    fn time_now() -> InputTime {
        InputTime::now()
    }

    /// Converts a logical pixel point in the stream corodinate space into a unit interval point in
    /// the global bounding rectangle.
    ///
    /// - `stream_path`: D-Bus object path like `/org/gnome/Mutter/ScreenCast/Stream/u7`
    async fn convert_stream_coordinate_space(
        &self,
        stream_path: &str,
        x: f64,
        y: f64,
    ) -> fdo::Result<Point<f64, UnitIntervalPointKind>> {
        let Some((screen_cast_iface, screen_cast_object_server)) = &self.screen_cast_session else {
            return Err(fdo::Error::Failed(
                "Must have screencast session for absolute coordinates".to_owned(),
            ));
        };

        let Ok(stream) = screen_cast_object_server
            .interface::<_, super::mutter_screen_cast::Stream>(stream_path)
            .await
        else {
            return Err(fdo::Error::Failed("Unknown stream".to_owned()));
        };

        let ipc_outputs = screen_cast_iface.get().await.ipc_outputs.clone();
        let stream = stream.get().await;
        let stream_geometry = {
            let ipc_outputs = ipc_outputs.lock().unwrap();
            stream.logical_geometry(&ipc_outputs)
        }
        .ok_or_else(|| {
            fdo::Error::Failed(
                "Stream target has no current output geometry; absolute coordinates are unsupported"
                    .to_owned(),
            )
        })?;

        // Absolute position in the global bounding rectangle, in logical pixels.
        let point_abs =
            Point::<f64, Logical>::new(stream_geometry.x as f64 + x, stream_geometry.y as f64 + y);

        let Some(output_geo) = ({
            let ipc_outputs = ipc_outputs.lock().unwrap();
            global_bounding_rectangle_ipc(&ipc_outputs)
        }) else {
            return Err(fdo::Error::Failed(
                "Missing outputs for getting global bounding rectangle".to_owned(),
            ));
        };

        if output_geo.size.w <= 0 || output_geo.size.h <= 0 {
            return Err(fdo::Error::Failed(
                "Global output geometry has no usable area".to_owned(),
            ));
        }

        let point_unit_interval =
            (point_abs - output_geo.loc.to_f64()).to_size() / output_geo.size.to_f64();
        Ok(Point::new(point_unit_interval.x, point_unit_interval.y))
    }

    /// Stops the session.
    pub async fn stop(&mut self, server: &ObjectServer, ctxt: &SignalEmitter<'_>) {
        self.release_held_input();
        let was_active = self.active;
        self.active = false;
        if was_active {
            if let Err(err) = self
                .to_calloop
                .send(RemoteDesktopDBusToCalloop::SetSessionActive {
                    session_id: self.id,
                    active: false,
                })
            {
                warn!("error sending session deactivation to calloop: {err:?}");
            }
        }
        self.stop_input();

        #[cfg(feature = "xdp-gnome-screencast")]
        {
            // Remove reference to this interface so it can be dropped
            self.shared.lock().await.sessions.remove(&self.id);

            if let Some((iface, server)) = &self.screen_cast_session {
                iface
                    .get_mut()
                    .await
                    .stop_no_remote_desktop(server, iface.signal_emitter())
                    .await;
            }

            // Remove reference to the screencast interface so it can be dropped
            self.screen_cast_session = None;
        }

        Self::closed(ctxt).await.unwrap();

        let obj_was_destroyed = server.remove::<Session, _>(ctxt.path()).await.unwrap();
        trace!(
            obj_was_destroyed,
            "removed RemoteDesktop.Session id={} from server",
            self.id
        );
    }
}

#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct ClipboardOptions {
    #[zvariant(rename = "mime-types")]
    _mime_types: Option<Vec<String>>,
}

#[derive(Debug, SerializeDict, Type)]
#[zvariant(signature = "dict")]
struct SelectionOwnerChangedOptions {
    #[zvariant(rename = "mime-types")]
    mime_types: Option<Vec<String>>,
    #[zvariant(rename = "session-is-owner")]
    session_is_owner: Option<bool>,
}

#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct ConnectToEisOptions {
    /// Bitflags of device types to expose and filter in EIS.
    ///
    /// Must be in `SupportedDeviceTypes` and is based on user's choice via portal
    #[zvariant(rename = "device-types")]
    device_types: Option<BitFlags<MutterXdpDeviceType>>,
}
#[derive(Serialize, Deserialize, Debug, Type, PartialEq, Eq, Clone, Copy)]
pub struct MutterXdpPointerAxisFlags(u32);

bitflags! {
    impl MutterXdpPointerAxisFlags: u32 {
        /// Note: only this is currently provided by xdp-gnome
        const FINISH = 1;
        const SOURCE_WHEEL = 1 << 1;
        const SOURCE_FINGER = 1 << 2;
        const SOURCE_CONTINUOUS = 1 << 3;
    }
}

#[derive(Serialize, Deserialize, Debug, Type, PartialEq, Eq, Clone, Copy)]
#[enumflags2::bitflags]
#[repr(u32)]
pub enum MutterXdpDeviceType {
    Keyboard = 1,
    Pointer = 1 << 1,
    Touchscreen = 1 << 2,
}

impl MutterXdpDeviceType {
    /// To [`reis`] capabilities for exposing only portal-selected device capabilities
    pub fn to_reis_capabilities(flags: BitFlags<Self>) -> BitFlags<reis::event::DeviceCapability> {
        use reis::event::DeviceCapability;
        let mut out_flags = BitFlags::empty();
        for flag in flags {
            match flag {
                MutterXdpDeviceType::Keyboard => out_flags |= DeviceCapability::Keyboard,
                MutterXdpDeviceType::Pointer => {
                    out_flags |= DeviceCapability::Pointer
                        | DeviceCapability::Scroll
                        | DeviceCapability::Button
                        | DeviceCapability::PointerAbsolute
                }
                MutterXdpDeviceType::Touchscreen => out_flags |= DeviceCapability::Touch,
            }
        }
        out_flags
    }
}

#[interface(
    name = "org.gnome.Mutter.RemoteDesktop.Session",
    spawn = false,
    introspection_docs = false
)]
impl Session {
    #[zbus(property)]
    async fn session_id(&self) -> &str {
        &self.id_str
    }

    async fn start(
        &mut self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        debug!("RemoteDesktop.Start id={}", self.id);

        if self.active {
            return Err(fdo::Error::Failed(
                "Unable to start the session: Already started".to_owned(),
            ));
        }
        self.active = true;
        if let Err(err) = self
            .to_calloop
            .send(RemoteDesktopDBusToCalloop::SetSessionActive {
                session_id: self.id,
                active: true,
            })
        {
            self.active = false;
            return Err(fdo::Error::Failed(format!(
                "Unable to activate the session: {err}"
            )));
        }

        #[cfg(feature = "xdp-gnome-screencast")]
        if let Some((iface, _server)) = &self.screen_cast_session {
            iface.get().await.start().await;
            debug!("RemoteDesktop.Start started screencast");
        }

        Ok(())
    }

    #[zbus(name = "Stop")]
    pub async fn stop_dbus(
        &mut self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_context)] ctxt: SignalEmitter<'_>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        debug!("RemoteDesktop.Stop id={}", self.id);

        self.stop(server, &ctxt).await;

        Ok(())
    }

    /// "A session doesn't have to have been started before it may be closed. After it being
    /// closed, it can no longer be used."
    #[zbus(signal)]
    async fn closed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    //// Keyboard handlers

    async fn notify_keyboard_keycode(
        &mut self,
        keycode: u32,
        state_is_pressed: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        self.emulate_input(InputEvent::Keyboard {
            event: self.wrap_event(RdKeyboardKeyEvent {
                // Offset from evdev keycodes (where KEY_ESCAPE is 1) to X11 keycodes
                keycode: Keycode::new(keycode + 8),
                state: if state_is_pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                },
            }),
        });
        Ok(())
    }

    async fn notify_keyboard_keysym(
        &mut self,
        keysym: u32,
        state_is_pressed: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        self.authorize_input()?;
        if let Err(err) = self
            .to_calloop
            .send(RemoteDesktopDBusToCalloop::EmulateKeysym {
                keysym,
                state: if state_is_pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                },
                session_id: self.id,
                time: Self::time_now(),
                keyboard_source: self.keyboard_source,
                release: false,
            })
        {
            warn!("error sending EmulateKeysym to calloop: {err:?}");
        } else if state_is_pressed {
            self.held_keysyms.insert(keysym);
        } else {
            self.held_keysyms.remove(&keysym);
        }
        Ok(())
    }

    //// Pointer handlers

    async fn notify_pointer_button(
        &mut self,
        button: i32,
        state: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        self.emulate_input(InputEvent::PointerButton {
            event: self.wrap_event(RdPointerButtonEvent { button, state }),
        });
        Ok(())
    }

    async fn notify_pointer_axis(
        &mut self,
        mut dx: f64,
        mut dy: f64,
        flags: MutterXdpPointerAxisFlags,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        let finish = flags.contains(MutterXdpPointerAxisFlags::FINISH);

        let source = if flags.contains(MutterXdpPointerAxisFlags::SOURCE_WHEEL) {
            AxisSource::Wheel
        } else if flags.contains(MutterXdpPointerAxisFlags::SOURCE_FINGER) {
            AxisSource::Finger
        } else if flags.contains(MutterXdpPointerAxisFlags::SOURCE_CONTINUOUS) {
            AxisSource::Continuous
        } else {
            // Mutter defaults to finger when no source flag is specified.
            AxisSource::Finger
        };

        if finish && matches!(source, AxisSource::Finger | AxisSource::Continuous) {
            // Niri detects axis stop from a zero delta for finger and continuous sources.
            dx = 0.0;
            dy = 0.0;
        }

        self.emulate_input(InputEvent::PointerAxis {
            event: self.wrap_event(RdPointerAxisEvent {
                source,
                discrete: None,
                delta: Some((dx, dy)),
            }),
        });

        Ok(())
    }

    async fn notify_pointer_axis_discrete(
        &mut self,
        axis: u32,
        steps: i32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        debug!(axis, steps);
        self.emulate_input(InputEvent::PointerAxis {
            event: self.wrap_event(RdPointerAxisEvent {
                source: AxisSource::Wheel,
                delta: None,
                discrete: Some(match axis {
                    0 => (0, steps),
                    _ => (steps, 0),
                }),
            }),
        });

        Ok(())
    }

    async fn notify_pointer_motion_relative(
        &mut self,
        dx: f64,
        dy: f64,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        self.emulate_input(InputEvent::PointerMotion {
            event: self.wrap_event(RdPointerMotionEvent { dx, dy }),
        });
        Ok(())
    }

    async fn notify_pointer_motion_absolute(
        &mut self,
        stream: &str,
        x: f64,
        y: f64,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        let pos = self.convert_stream_coordinate_space(stream, x, y).await?;

        self.emulate_input(InputEvent::PointerMotionAbsolute {
            event: self.wrap_event(RdPointerMotionAbsoluteEvent(RdAbsolutePosition { pos })),
        });
        Ok(())
    }

    //// Touch handlers

    async fn notify_touch_down(
        &mut self,
        stream: &str,
        slot: u32,
        x: f64,
        y: f64,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        let pos = self.convert_stream_coordinate_space(stream, x, y).await?;

        self.emulate_touch_input(
            InputEvent::TouchDown {
                event: self.wrap_event(RdTouchEvent {
                    slot,
                    extra: RdAbsolutePosition { pos },
                }),
            },
            slot,
        );
        Ok(())
    }

    async fn notify_touch_motion(
        &mut self,
        stream: &str,
        slot: u32,
        x: f64,
        y: f64,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        let pos = self.convert_stream_coordinate_space(stream, x, y).await?;

        self.emulate_touch_input(
            InputEvent::TouchMotion {
                event: self.wrap_event(RdTouchEvent {
                    slot,
                    extra: RdAbsolutePosition { pos },
                }),
            },
            slot,
        );
        Ok(())
    }

    async fn notify_touch_up(
        &mut self,
        slot: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<()> {
        authorize_session_request(self, connection, &header).await?;
        self.emulate_touch_input(
            InputEvent::TouchUp {
                event: self.wrap_event(RdTouchEvent { slot, extra: () }),
            },
            slot,
        );
        Ok(())
    }

    //// Clipboard

    /// Enables calling *Selection* and DisableClipboard
    async fn enable_clipboard(&mut self, _options: ClipboardOptions) -> fdo::Result<()> {
        Err(fdo::Error::Failed(
            "Clipboard sharing is not supported".to_owned(),
        ))
    }

    #[zbus(name = "DisableClipboard")]
    async fn disable_clipboard(&mut self) -> fdo::Result<()> {
        Err(fdo::Error::Failed(
            "Clipboard sharing is not supported".to_owned(),
        ))
    }

    async fn set_selection(&mut self, _options: ClipboardOptions) -> fdo::Result<()> {
        Err(fdo::Error::Failed(
            "Clipboard sharing is not supported".to_owned(),
        ))
    }

    /// Answer to the [`selection_transfer`] signal.
    async fn selection_write(&mut self, _serial: u32) -> fdo::Result<zvariant::OwnedFd> {
        Err(fdo::Error::Failed(
            "Clipboard sharing is not supported".to_owned(),
        ))
    }

    async fn selection_write_done(&mut self, _serial: u32, _success: bool) -> fdo::Result<()> {
        Err(fdo::Error::Failed(
            "Clipboard sharing is not supported".to_owned(),
        ))
    }

    async fn selection_read(&mut self, _mime_type: &str) -> fdo::Result<zvariant::OwnedFd> {
        Err(fdo::Error::Failed(
            "Clipboard sharing is not supported".to_owned(),
        ))
    }

    #[zbus(signal)]
    async fn selection_owner_changed(
        ctxt: &SignalEmitter<'_>,
        options: SelectionOwnerChangedOptions,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn selection_transfer(
        ctxt: &SignalEmitter<'_>,
        mime_type: &str,
        serial: u32,
    ) -> zbus::Result<()>;

    //// Properties

    #[zbus(property)]
    async fn caps_lock_state(&self) -> fdo::Result<bool> {
        Err(fdo::Error::Failed("CapsLockState is deprecated and not used by xdg-desktop-portal-gnome. Because of that it's not implemented by Niri.".to_owned()))
    }
    #[zbus(property)]
    async fn num_lock_state(&self) -> fdo::Result<bool> {
        Err(fdo::Error::Failed("NumLockState is deprecated and not used by xdg-desktop-portal-gnome. Because of that it's not implemented by Niri.".to_owned()))
    }

    //// EIS

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &mut self,
        options: ConnectToEisOptions,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<zvariant::OwnedFd> {
        authorize_session_request(self, connection, &header).await?;
        self.authorize_input()?;
        if self.using_eis {
            // Mutter supports calling meta_eis_add_client_get_fd multiple times and also
            // checks for EIS existence in the Start handler.

            // xdp spec: "This method may only be called once per session, where the EIS
            // implementation disconnects the session should be closed."
            return Err(fdo::Error::Failed(
                "Unable to ConnectToEIS: Already gave EIS socket".to_owned(),
            ));
        }
        let (a, b) = UnixStream::pair().map_err(|err| {
            fdo::Error::Failed(format!("Unable to create EIS socket pair: {err}"))
        })?;
        let ctx = reis::eis::Context::new(a)
            .map_err(|err| fdo::Error::Failed(format!("Unable to create EIS context: {err}")))?;

        debug!("RemoteDesktop.ConnectToEIS");

        let (setup_result, setup_result_rx) = async_channel::bounded(1);
        self.to_calloop
            .send(RemoteDesktopDBusToCalloop::NewEisContext {
                session_id: self.id,
                ctx,
                exposed_device_types: options.device_types.unwrap_or_else(BitFlags::all),
                setup_result,
            })
            .map_err(|err| {
                fdo::Error::Failed(format!("Unable to dispatch EIS context to calloop: {err}"))
            })?;

        setup_result_rx
            .recv()
            .await
            .map_err(|err| {
                fdo::Error::Failed(format!("EIS context setup did not complete: {err}"))
            })?
            .map_err(fdo::Error::Failed)?;

        self.using_eis = true;
        Ok(OwnedFd::from(b).into())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        debug!("RemoteDesktop.Session id={} is being dropped", self.id);
        self.release_held_input();
        if self.active {
            let _ = self
                .to_calloop
                .send(RemoteDesktopDBusToCalloop::SetSessionActive {
                    session_id: self.id,
                    active: false,
                });
        }
        self.stop_input();
    }
}
#[cfg(test)]
mod authorization_tests {
    use super::{is_authorized_portal_sender, OwnedUniqueName};

    #[test]
    fn portal_session_is_limited_to_creator_that_still_owns_backend_name() {
        let creator = OwnedUniqueName::try_from(":1.42").unwrap();
        let other_client = OwnedUniqueName::try_from(":1.43").unwrap();

        assert!(is_authorized_portal_sender(
            &creator,
            Some(&creator),
            &creator
        ));
        assert!(!is_authorized_portal_sender(
            &other_client,
            Some(&creator),
            &creator
        ));
        assert!(!is_authorized_portal_sender(
            &creator,
            Some(&creator),
            &other_client
        ));
    }
}
