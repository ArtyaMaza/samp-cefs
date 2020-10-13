use crate::app::{Event, ExternalBrowser};
use crate::audio::Audio;
use crate::browser::client::WebClient;
use crate::external::{BrowserReadyCallback, CallbackList};

use cef::handlers::render::PaintElement;
use cef::types::list::{List, ValueType};
use cef::types::string::CefString;
use cef_sys::{cef_event_flags_t, cef_key_event_t, cef_mouse_button_type_t, cef_mouse_event_t};

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, MutexGuard};

use client_api::gta::rw::rwcore::{RwRaster, RwTexture};
use client_api::gta::rw::rwplcore::RwSurfaceProperties;

use crossbeam_channel::Sender;

#[derive(Debug, Clone, Copy, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub enum MouseKey {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Default)]
struct Mouse {
    x: i32,
    y: i32,
    keys: HashMap<MouseKey, bool>,
}

#[derive(Clone)]
pub struct ExternalClient {
    pub browser: Arc<WebClient>,
    pub object_ids: Vec<i32>,
    pub texture: String,
    pub scale: i32,
    pub origin_texture: *mut RwTexture,
    pub origin_surface_props: RwSurfaceProperties,
    pub prev_replacement: *mut RwTexture,
}

pub struct Manager {
    clients: HashMap<u32, Arc<WebClient>>,
    audio: Arc<Audio>,
    ready_callbacks: HashMap<u32, Vec<BrowserReadyCallback>>,
    clients_on_txd: Vec<ExternalClient>,
    focused: Option<u32>,
    focused_queue: VecDeque<u32>,
    input_corrupted: bool,
    do_not_draw: bool,
    event_tx: Sender<Event>,
    mouse: Mouse,
    view_width: usize,
    view_height: usize,
    prev_fps: u64,
}

impl Manager {
    pub fn new(event_tx: Sender<Event>, audio: Arc<Audio>) -> Manager {
        // init cef
        crate::browser::cef::initialize(event_tx.clone());

        let mut keys = HashMap::new();

        keys.insert(MouseKey::Left, false);
        keys.insert(MouseKey::Middle, false);
        keys.insert(MouseKey::Right, false);

        let mouse = Mouse { x: 0, y: 0, keys };

        Manager {
            clients: HashMap::new(),
            ready_callbacks: HashMap::new(),
            clients_on_txd: Vec::new(),
            view_height: 0,
            view_width: 0,
            prev_fps: 60,
            input_corrupted: false,
            do_not_draw: false,
            focused: None,
            focused_queue: VecDeque::new(),
            audio,
            mouse,
            event_tx,
        }
    }

    pub fn create_browser(&mut self, id: u32, cbs: CallbackList, url: &str) {
        let client = WebClient::new(id, cbs, self.event_tx.clone());
        crate::browser::cef::create_browser(client.clone(), url);
        self.append_client(id, client);
    }

    pub fn create_browser_on_texture(&mut self, ext: &ExternalBrowser, cbs: CallbackList) {
        let client = WebClient::new_extern(ext.id, cbs, self.event_tx.clone(), self.audio.clone());
        crate::browser::cef::create_browser(client.clone(), &ext.url);
        self.append_client(ext.id, client.clone());

        let ext_client = ExternalClient {
            browser: client,
            texture: ext.texture.clone(),
            object_ids: Vec::new(),
            scale: ext.scale,
            origin_texture: std::ptr::null_mut(),
            origin_surface_props: RwSurfaceProperties {
                ambient: 0.0,
                diffuse: 0.0,
                specular: 0.0,
            },
            prev_replacement: std::ptr::null_mut(),
        };

        self.clients_on_txd.push(ext_client);
    }

    #[inline]
    pub fn browser_append_to_object(&mut self, id: u32, object_id: i32) {
        self.audio.add_source(id, object_id);

        self.clients_on_txd
            .iter_mut()
            .filter(|cl| cl.browser.id() == id)
            .for_each(|cl| {
                cl.object_ids.push(object_id);
                cl.browser.add_object(object_id);

                if cl.object_ids.len() == 1 {
                    cl.browser.hide(false);
                }
            });
    }

    #[inline]
    pub fn browser_remove_from_object(&mut self, id: u32, object_id: i32) {
        self.audio.remove_source(id, object_id);

        self.clients_on_txd
            .iter_mut()
            .filter(|cl| cl.browser.id() == id)
            .for_each(|cl| {
                let mut idx = 0;

                while idx < cl.object_ids.len() {
                    if cl.object_ids[idx] == object_id {
                        cl.object_ids.remove(idx);
                        cl.browser.remove_object(object_id);
                    } else {
                        idx += 1;
                    }
                }

                if cl.object_ids.len() == 0 {
                    cl.browser.hide(true);
                }
            });
    }

    #[inline]
    fn append_client(&mut self, id: u32, client: Arc<WebClient>) {
        if let Some(client) = self.clients.insert(id, client) {
            self.internal_close(client, true);
        }
    }

    #[inline]
    pub fn draw(&self) {
        if self.do_not_draw {
            return;
        }

        if let Some(&focus) = self.focused.as_ref() {
            for client in self.clients.values().filter(|client| client.id() != focus) {
                client.update_view();
                client.draw();
            }

            if let Some(focused) = self.clients.get(&focus) {
                focused.update_view();
                focused.draw();
            }
        } else {
            for client in self.clients.values() {
                client.update_view();
                client.draw();
            }
        }
    }

    #[inline]
    pub fn on_lost_device(&mut self) {
        for (_, browser) in &self.clients {
            browser.on_lost_device();

            if browser.is_extern() {
                self.clients_on_txd
                    .iter_mut()
                    .filter(|client| client.browser.id() == browser.id())
                    .for_each(|client| client.origin_texture = std::ptr::null_mut());
            }
        }
    }

    #[inline]
    pub fn on_reset_device(&self) {
        for (_, client) in &self.clients {
            client.on_reset_device();

            client
                .browser()
                .map(|browser| browser.host())
                .map(|host| host.invalidate(PaintElement::View));
        }
    }

    #[inline]
    pub fn resize(&mut self, width: usize, height: usize) {
        if width == self.view_width && height == self.view_height {
            return;
        }

        self.view_width = width;
        self.view_height = height;

        for (_, browser) in &self.clients {
            if !browser.is_extern() {
                browser.resize(width, height);
            }
        }
    }

    #[inline]
    pub fn send_mouse_move_event(&mut self, x: i32, y: i32) {
        if self.input_corrupted {
            return;
        }

        if let Some(client) = self.focused.as_ref().and_then(|id| self.clients.get(id)) {
            if let Some(host) = client.browser().map(|browser| browser.host()) {
                self.mouse.x = x;
                self.mouse.y = y;

                let keys = &self.mouse.keys;

                let mut event = cef_mouse_event_t { x, y, modifiers: 0 };

                if keys.get(&MouseKey::Left).cloned().unwrap_or(false) {
                    event.modifiers |= cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON as u32;
                }

                if keys.get(&MouseKey::Middle).cloned().unwrap_or(false) {
                    event.modifiers |= cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON as u32;
                }

                if keys.get(&MouseKey::Right).cloned().unwrap_or(false) {
                    event.modifiers |= cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON as u32;
                }

                host.send_mouse_move(event);
            }
        }
    }

    #[inline]
    pub fn send_mouse_click_event(&mut self, button: MouseKey, is_down: bool) {
        if self.input_corrupted {
            return;
        }

        if let Some(client) = self.focused.as_ref().and_then(|id| self.clients.get(id)) {
            if let Some(host) = client.browser().map(|browser| browser.host()) {
                self.mouse.keys.insert(button, is_down);

                let event = cef_mouse_event_t {
                    x: self.mouse.x,
                    y: self.mouse.y,
                    modifiers: 0,
                };

                let key = match button {
                    MouseKey::Left => cef_mouse_button_type_t::MBT_LEFT,
                    MouseKey::Middle => cef_mouse_button_type_t::MBT_MIDDLE,
                    MouseKey::Right => cef_mouse_button_type_t::MBT_RIGHT,
                };

                host.send_mouse_click(key, event, is_down);
            }
        }
    }

    #[inline]
    pub fn send_mouse_wheel(&self, delta: i32) {
        if self.input_corrupted {
            return;
        }

        if let Some(client) = self.focused.as_ref().and_then(|id| self.clients.get(id)) {
            if let Some(host) = client.browser().map(|browser| browser.host()) {
                host.send_mouse_wheel(self.mouse.x, self.mouse.y, delta);
            }
        }
    }

    #[inline]
    pub fn send_keyboard_event(&self, event: cef_key_event_t) {
        if self.input_corrupted {
            return;
        }

        if let Some(client) = self.focused.as_ref().and_then(|id| self.clients.get(id)) {
            if let Some(host) = client.browser().map(|browser| browser.host()) {
                host.send_keyboard_event(event.clone());
            }
        }
    }

    pub fn trigger_event(&self, event_name: &str, list: List) {
        for client in self.clients.values() {
            if let Some(frame) = client.browser().map(|browser| browser.main_frame()) {
                let name = CefString::new(event_name);
                let msg = cef::process_message::ProcessMessage::create("trigger_event");

                let args = msg.argument_list();
                args.set_string(0, &name);
                args.set_list(1, list.clone());

                frame.send_process_message(cef::ProcessId::Renderer, msg);
            }
        }
    }

    pub fn close_browser(&mut self, id: u32, force_close: bool) {
        if let Some(client) = self.clients.remove(&id) {
            self.internal_close(client, force_close);
        }
    }

    #[inline]
    pub fn hide_browser(&self, id: u32, hide: bool) {
        if let Some(browser) = self.clients.get(&id) {
            browser.hide(hide);
        }
    }

    pub fn browser_focus(&mut self, id: u32, focus: bool) {
        if self.clients.contains_key(&id) {
            if focus {
                if let Some(&cur_id) = self.focused.as_ref() {
                    if cur_id != id {
                        self.focused_queue.push_back(id);
                    }
                } else {
                    self.focused = Some(id);
                }
            } else {
                if let Some(_) = self.focused.as_ref().filter(|focused| **focused == id) {
                    self.focused = self.focused_queue.pop_front();
                } else {
                    self.focused_queue
                        .iter()
                        .position(|&queue| queue == id)
                        .map(|idx| self.focused_queue.remove(idx));
                }
            }
        }
    }

    #[inline(always)]
    pub fn is_input_blocked(&self) -> bool {
        self.focused.is_some()
    }

    #[inline(always)]
    pub fn is_input_available(&self, browser: u32) -> bool {
        if self.input_corrupted {
            return false;
        }

        if self.is_input_blocked() {
            self.focused.as_ref().filter(|&&id| id == browser).is_some()
        } else {
            true
        }
    }

    #[inline(always)]
    pub fn set_corrupted(&mut self, corrupted: bool) {
        self.input_corrupted = corrupted;
    }

    #[inline(always)]
    pub fn is_input_corrupted(&self) -> bool {
        self.input_corrupted
    }

    #[inline(always)]
    pub fn do_not_draw(&mut self, donot: bool) {
        if self.do_not_draw != donot {
            self.do_not_draw = donot;
            self.temporary_hide(donot);
        }
    }

    #[inline(always)]
    pub fn browser_exists(&self, browser_id: u32) -> bool {
        self.clients.contains_key(&browser_id)
    }

    #[inline(always)]
    pub fn browser_ready(&self, browser_id: u32) -> bool {
        self.clients
            .get(&browser_id)
            .and_then(|client| client.browser())
            .map(|browser| !browser.is_loading())
            .unwrap_or(false)
    }

    pub fn toggle_dev_tools(&self, browser_id: u32, enabled: bool) {
        self.clients
            .get(&browser_id)
            .map(|client| client.toggle_dev_tools(enabled));
    }

    pub fn call_browser_ready(&self, browser_id: u32) {
        self.ready_callbacks
            .get(&browser_id)
            .map(|callbacks| callbacks.iter().for_each(|cb| cb(browser_id)));
    }

    pub fn add_browser_ready(&mut self, browser_id: u32, callback: BrowserReadyCallback) {
        if self.browser_ready(browser_id) {
            callback(browser_id);
            return;
        }

        self.ready_callbacks
            .entry(browser_id)
            .or_insert_with(|| Vec::new())
            .push(callback);
    }

    #[inline(always)]
    pub fn external_browsers(&mut self) -> &mut [ExternalClient] {
        &mut self.clients_on_txd
    }

    #[inline(always)]
    pub fn update_fps(&mut self, fps: u64) {
        let fps_small = fps as i32;
        let fps_small = std::cmp::max(15, fps_small); // std::cmp::min(60, fps_small)

        for browser in self.clients.values().filter_map(|client| client.browser()) {
            let host = browser.host();

            if self.prev_fps != fps {
                host.set_windowless_frame_rate(fps_small);
                self.prev_fps = fps;
            }
        }
    }

    pub fn close_all_browsers(&mut self) {
        self.clients_on_txd.clear();
        let audio = self.audio.clone();

        self.clients
            .drain()
            .for_each(|(_, client)| Self::internal_close_client(client, &audio, true));
    }

    #[inline]
    fn temporary_hide(&self, hide: bool) {
        for client in self.clients.values() {
            if hide {
                client.internal_hide(true, false);
            } else {
                client.restore_hide_status();
            }
        }
    }

    fn internal_close(&mut self, client: Arc<WebClient>, force_close: bool) {
        if client.is_extern() {
            if let Some(idx) = self
                .clients_on_txd
                .iter()
                .position(|cl| cl.browser.id() == client.id())
            {
                self.clients_on_txd.remove(idx);
            }
        }

        Self::internal_close_client(client, &self.audio, force_close);
    }

    fn internal_close_client(client: Arc<WebClient>, audio: &Arc<Audio>, force_close: bool) {
        // println!("internal_close_client");
        client.remove_view();
        client.close(force_close);
        audio.remove_all_streams(client.id());
        // println!("internal_close_client end");
    }
}
