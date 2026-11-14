//! Minimal FFI bridge to OpenTUI's native Zig renderer.
//!
//! Only the plugin surface uses this module. It loads `opentui.dll` at runtime
//! and drives the retained scene, Yoga layout and cell buffer directly; no Bun
//! process or JavaScript runtime is involved.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::Rgba;

const ABI_VERSION: u32 = 2;
const OT_OK: i32 = 0;
const OT_SCENE_ROOT: u32 = 0;
const OT_SCENE_BOX: u32 = 1;
const OT_SCENE_TEXT: u32 = 2;
const OT_SESSION_BUFFER_NEXT: u32 = 1;
const OT_SCENE_WRAP_NONE: u32 = 0;
const OT_SCENE_WRAP_CHAR: u32 = 1;
const OT_SCENE_WRAP_WORD: u32 = 2;
const OT_SCENE_ALIGN_LEFT: u32 = 0;
const OT_SCENE_ALIGN_CENTER: u32 = 1;
const OT_SCENE_ALIGN_RIGHT: u32 = 2;
const OT_BORDER_ALL: u32 = 15;
const OT_BORDER_SINGLE: u32 = 0;
const OT_BORDER_DOUBLE: u32 = 1;
const OT_BORDER_ROUNDED: u32 = 2;
const OT_BORDER_HEAVY: u32 = 3;
const OT_ATTR_BOLD: u32 = 1;
const OT_ATTR_DIM: u32 = 2;
const OT_ATTR_ITALIC: u32 = 4;
const OT_ATTR_UNDERLINE: u32 = 8;
const OT_ATTR_INVERSE: u32 = 32;
const OT_ATTR_STRIKETHROUGH: u32 = 128;
const OT_CELL_CHAR_KIND_MASK: u32 = 0xC000_0000;
const OT_CELL_CHAR_CONTINUATION: u32 = 0xC000_0000;
const MAX_CAPTURE_HEIGHT: u32 = 400;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct OtHandle {
    context_id: u64,
    slot: u32,
    generation: u32,
}

pub type SceneHandle = OtHandle;

#[repr(C)]
#[derive(Clone, Copy)]
struct ContextOptions {
    struct_size: u32,
    abi_version: u32,
    flags: u32,
    object_capacity: u32,
    render_cells_max: u32,
    reserved: [u32; 3],
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            struct_size: std::mem::size_of::<Self>() as u32,
            abi_version: ABI_VERSION,
            flags: 0,
            object_capacity: 16_384,
            render_cells_max: 512 * 512,
            reserved: [0; 3],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SessionOptions {
    struct_size: u32,
    abi_version: u32,
    chunk_size: u32,
    span_capacity: u32,
    max_bytes: u64,
    control_capacity: u32,
    reserved: u32,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            struct_size: std::mem::size_of::<Self>() as u32,
            abi_version: ABI_VERSION,
            chunk_size: 64 * 1024,
            span_capacity: 64,
            max_bytes: 4 * 1024 * 1024,
            control_capacity: 4096,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RendererOptions {
    struct_size: u32,
    abi_version: u32,
    width: u32,
    height: u32,
    remote: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PaintOptions {
    struct_size: u32,
    abi_version: u32,
    z_index: i32,
    opacity: f32,
    translate_x: f64,
    translate_y: f64,
    border_sides: u32,
    should_fill: u32,
    background: [u16; 4],
    border_color: [u16; 4],
    border_style: u32,
    focusable: u32,
    focused_border_color: [u16; 4],
    reserved: u32,
}

impl Default for PaintOptions {
    fn default() -> Self {
        Self {
            struct_size: std::mem::size_of::<Self>() as u32,
            abi_version: ABI_VERSION,
            z_index: 0,
            opacity: 1.0,
            translate_x: 0.0,
            translate_y: 0.0,
            border_sides: 0,
            should_fill: 0,
            background: [0; 4],
            border_color: [0; 4],
            border_style: OT_BORDER_SINGLE,
            focusable: 0,
            focused_border_color: [0, 170, 255, 255],
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TextOptions {
    struct_size: u32,
    abi_version: u32,
    foreground: [u16; 4],
    background: [u16; 4],
    attributes: u32,
    wrap_mode: u32,
    truncate: u32,
    first_line_offset: u32,
    scroll_x: f64,
    scroll_y: f64,
    tab_indicator: u32,
    tab_color_set: u32,
    tab_color: [u16; 4],
    text_align: u32,
}

impl Default for TextOptions {
    fn default() -> Self {
        Self {
            struct_size: std::mem::size_of::<Self>() as u32,
            abi_version: ABI_VERSION,
            foreground: [255, 255, 255, 255],
            background: [0, 0, 0, 0],
            attributes: 0,
            wrap_mode: OT_SCENE_WRAP_WORD,
            truncate: 0,
            first_line_offset: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            tab_indicator: 0,
            tab_color_set: 0,
            tab_color: [0; 4],
            text_align: OT_SCENE_ALIGN_LEFT,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StyledTextChunk {
    struct_size: u32,
    abi_version: u32,
    byte_count: u32,
    flags: u32,
    foreground: [u16; 4],
    background: [u16; 4],
    attributes: u32,
    reserved: u32,
    link_offset: u32,
    link_byte_count: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BoxDetails {
    struct_size: u32,
    abi_version: u32,
    title_alignment: u32,
    bottom_title_alignment: u32,
    flags: u32,
    reserved: u32,
    title_color: [u16; 4],
    border_characters: [u32; 11],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SceneLayout {
    struct_size: u32,
    abi_version: u32,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    width: f32,
    height: f32,
    screen_x: f64,
    screen_y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FrameRequest {
    struct_size: u32,
    abi_version: u32,
    session: OtHandle,
    root: OtHandle,
    node: OtHandle,
    frame_id: u64,
    request_id: u64,
    layout_epoch: u64,
    hook_generation: u64,
    kind: u32,
    num: u32,
    width: u32,
    height: u32,
    reserved: [u32; 2],
}

impl Default for FrameRequest {
    fn default() -> Self {
        Self {
            struct_size: std::mem::size_of::<Self>() as u32,
            abi_version: ABI_VERSION,
            session: OtHandle::default(),
            root: OtHandle::default(),
            node: OtHandle::default(),
            frame_id: 0,
            request_id: 0,
            layout_epoch: 0,
            hook_generation: 0,
            kind: 0,
            num: 0,
            width: 0,
            height: 0,
            reserved: [0; 2],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BufferLeaseSnapshot {
    struct_size: u32,
    abi_version: u32,
    lease: OtHandle,
    width: u32,
    height: u32,
    generation: u64,
    char_ptr: u64,
    fg_ptr: u64,
    bg_ptr: u64,
    attributes_ptr: u64,
    reserved: u64,
}

impl Default for BufferLeaseSnapshot {
    fn default() -> Self {
        Self {
            struct_size: std::mem::size_of::<Self>() as u32,
            abi_version: ABI_VERSION,
            lease: OtHandle::default(),
            width: 0,
            height: 0,
            generation: 0,
            char_ptr: 0,
            fg_ptr: 0,
            bg_ptr: 0,
            attributes_ptr: 0,
            reserved: 0,
        }
    }
}

type ContextAbiVersion = unsafe extern "C" fn() -> u32;
type ContextCreate = unsafe extern "C" fn(*const ContextOptions, *mut *mut c_void) -> i32;
type ContextDestroy = unsafe extern "C" fn(*mut c_void) -> i32;
type SessionCreate = unsafe extern "C" fn(*mut c_void, *const SessionOptions, *mut OtHandle) -> i32;
type SessionDestroy = unsafe extern "C" fn(*mut c_void, *const OtHandle) -> i32;
type SessionAttachRenderer =
    unsafe extern "C" fn(*mut c_void, *const OtHandle, *const RendererOptions) -> i32;
type SessionResizeRenderer = unsafe extern "C" fn(*mut c_void, *const OtHandle, u32, u32) -> i32;
type SceneCreateNode =
    unsafe extern "C" fn(*mut c_void, *const OtHandle, u32, u32, *mut OtHandle) -> i32;
type SceneDestroyNode = unsafe extern "C" fn(*mut c_void, *const OtHandle) -> i32;
type SceneMoveNode =
    unsafe extern "C" fn(*mut c_void, *const OtHandle, *const OtHandle, u32) -> i32;
type SceneSetStyle =
    unsafe extern "C" fn(*mut c_void, *const OtHandle, u32, u32, u32, u32, f32, u32) -> i32;
type SceneSetPaint = unsafe extern "C" fn(*mut c_void, *const OtHandle, *const PaintOptions) -> i32;
type SceneSetText = unsafe extern "C" fn(*mut c_void, *const OtHandle, *const u8, u32) -> i32;
type SceneSetStyledText = unsafe extern "C" fn(
    *mut c_void,
    *const OtHandle,
    *const u8,
    u32,
    *const StyledTextChunk,
    u32,
    *const u8,
    u32,
) -> i32;
type SceneSetTextOptions =
    unsafe extern "C" fn(*mut c_void, *const OtHandle, *const TextOptions) -> i32;
type SceneSetBoxDetails = unsafe extern "C" fn(
    *mut c_void,
    *const OtHandle,
    *const BoxDetails,
    *const u8,
    u32,
    *const u8,
    u32,
) -> i32;
type SceneMeasureLayout =
    unsafe extern "C" fn(*mut c_void, *const OtHandle, *const OtHandle) -> i32;
type SceneGetLayout =
    unsafe extern "C" fn(*mut c_void, *const OtHandle, u32, *mut SceneLayout) -> i32;
type ScenePaint = unsafe extern "C" fn(
    *mut c_void,
    *const OtHandle,
    *const u16,
    u32,
    u32,
    *mut FrameRequest,
) -> i32;
type FrameAcquireLease = unsafe extern "C" fn(
    *mut c_void,
    *const OtHandle,
    *const FrameRequest,
    u32,
    *mut BufferLeaseSnapshot,
) -> i32;
type FrameCancel = unsafe extern "C" fn(*mut c_void, *const OtHandle, u64) -> i32;
type LeaseCharSize = unsafe extern "C" fn(*mut c_void, *const OtHandle, u32, *mut u32) -> i32;
type LeaseWriteChars = unsafe extern "C" fn(
    *mut c_void,
    *const OtHandle,
    *mut u8,
    u32,
    u32,
    *mut u8,
    u32,
    *mut u32,
) -> i32;
type LeaseRelease = unsafe extern "C" fn(*mut c_void, *const OtHandle) -> i32;

struct Api {
    _library: libloading::Library,
    context_abi_version: ContextAbiVersion,
    context_create: ContextCreate,
    context_destroy: ContextDestroy,
    session_create: SessionCreate,
    session_destroy: SessionDestroy,
    session_attach_renderer: SessionAttachRenderer,
    session_resize_renderer: SessionResizeRenderer,
    scene_create_node: SceneCreateNode,
    scene_destroy_node: SceneDestroyNode,
    scene_move_node: SceneMoveNode,
    scene_set_style: SceneSetStyle,
    scene_set_paint: SceneSetPaint,
    scene_set_text: SceneSetText,
    scene_set_styled_text: SceneSetStyledText,
    scene_set_text_options: SceneSetTextOptions,
    scene_set_box_details: SceneSetBoxDetails,
    scene_measure_layout: SceneMeasureLayout,
    scene_get_layout: SceneGetLayout,
    scene_paint: ScenePaint,
    frame_acquire_lease: FrameAcquireLease,
    frame_cancel: FrameCancel,
    lease_char_size: LeaseCharSize,
    lease_write_chars: LeaseWriteChars,
    lease_release: LeaseRelease,
}

impl Api {
    fn load() -> Option<Self> {
        let library = load_library()?;
        unsafe {
            macro_rules! symbol {
                ($name:ident : $ty:ty) => {{
                    let symbol: libloading::Symbol<$ty> =
                        library.get(stringify!($name).as_bytes()).ok()?;
                    *symbol
                }};
            }
            let api = Self {
                context_abi_version: symbol!(ot_context_abi_version: ContextAbiVersion),
                context_create: symbol!(ot_context_create: ContextCreate),
                context_destroy: symbol!(ot_context_destroy: ContextDestroy),
                session_create: symbol!(ot_session_create: SessionCreate),
                session_destroy: symbol!(ot_session_destroy: SessionDestroy),
                session_attach_renderer: symbol!(
                    ot_session_attach_renderer: SessionAttachRenderer
                ),
                session_resize_renderer: symbol!(
                    ot_session_resize_renderer: SessionResizeRenderer
                ),
                scene_create_node: symbol!(ot_scene_create_node: SceneCreateNode),
                scene_destroy_node: symbol!(ot_scene_destroy_node: SceneDestroyNode),
                scene_move_node: symbol!(ot_scene_move_node: SceneMoveNode),
                scene_set_style: symbol!(ot_scene_set_style: SceneSetStyle),
                scene_set_paint: symbol!(ot_scene_set_paint: SceneSetPaint),
                scene_set_text: symbol!(ot_scene_set_text: SceneSetText),
                scene_set_styled_text: symbol!(
                    ot_scene_set_styled_text: SceneSetStyledText
                ),
                scene_set_text_options: symbol!(
                    ot_scene_set_text_options: SceneSetTextOptions
                ),
                scene_set_box_details: symbol!(
                    ot_scene_set_box_details: SceneSetBoxDetails
                ),
                scene_measure_layout: symbol!(
                    ot_scene_measure_layout: SceneMeasureLayout
                ),
                scene_get_layout: symbol!(ot_scene_get_layout: SceneGetLayout),
                scene_paint: symbol!(ot_scene_paint: ScenePaint),
                frame_acquire_lease: symbol!(
                    ot_scene_frame_acquire_buffer_lease: FrameAcquireLease
                ),
                frame_cancel: symbol!(ot_scene_frame_cancel: FrameCancel),
                lease_char_size: symbol!(
                    ot_buffer_lease_get_real_char_size: LeaseCharSize
                ),
                lease_write_chars: symbol!(
                    ot_buffer_lease_write_resolved_chars: LeaseWriteChars
                ),
                lease_release: symbol!(ot_buffer_lease_release: LeaseRelease),
                _library: library,
            };
            if (api.context_abi_version)() == ABI_VERSION {
                Some(api)
            } else {
                None
            }
        }
    }
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(Api::load).as_ref()
}

fn load_library() -> Option<libloading::Library> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(value) = std::env::var("OPENCODE_OPENTUI_DLL") {
        if !value.trim().is_empty() {
            paths.push(PathBuf::from(value));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("opentui.dll"));
            if let Some(parent) = dir.parent() {
                paths.push(parent.join("opentui.dll"));
            }
        }
    }
    paths.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("native")
            .join("x86_64-windows")
            .join("opentui.dll"),
    );
    for path in paths {
        if path.is_file() {
            if let Ok(library) = unsafe { libloading::Library::new(&path) } {
                return Some(library);
            }
        }
    }
    None
}

fn debug_status(operation: &str, status: i32) {
    if std::env::var_os("OPENCODE_NATIVE_DEBUG").is_some() {
        eprintln!("[opentui-native] {operation} -> {status}");
    }
}

/// A native scene built for one plugin render.
pub struct Scene {
    api: &'static Api,
    context: *mut c_void,
    session: OtHandle,
    root: OtHandle,
    width: u32,
    height: u32,
    next_num: u32,
    nodes: Vec<OtHandle>,
}

impl Scene {
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let api = api()?;
        let width = width.max(1);
        let height = height.clamp(1, MAX_CAPTURE_HEIGHT);
        unsafe {
            let mut context = std::ptr::null_mut();
            let options = ContextOptions::default();
            let status = (api.context_create)(&options, &mut context);
            if status != OT_OK || context.is_null() {
                debug_status("ot_context_create", status);
                return None;
            }
            let mut session = OtHandle::default();
            let session_options = SessionOptions::default();
            let status = (api.session_create)(context, &session_options, &mut session);
            if status != OT_OK {
                debug_status("ot_session_create", status);
                (api.context_destroy)(context);
                return None;
            }
            let renderer = RendererOptions {
                struct_size: std::mem::size_of::<RendererOptions>() as u32,
                abi_version: ABI_VERSION,
                width,
                height,
                remote: 1,
                reserved: 0,
            };
            let status = (api.session_attach_renderer)(context, &session, &renderer);
            if status != OT_OK {
                debug_status("ot_session_attach_renderer", status);
                (api.session_destroy)(context, &session);
                (api.context_destroy)(context);
                return None;
            }
            let mut root = OtHandle::default();
            let status = (api.scene_create_node)(context, &session, OT_SCENE_ROOT, 1, &mut root);
            if status != OT_OK {
                debug_status("ot_scene_create_node(root)", status);
                (api.session_destroy)(context, &session);
                (api.context_destroy)(context);
                return None;
            }
            let mut scene = Self {
                api,
                context,
                session,
                root,
                width,
                height,
                next_num: 2,
                nodes: vec![root],
            };
            scene.layout(root, 2, 0, 0, 2, 100.0, 0);
            scene.layout(root, 2, 1, 0, 2, 100.0, 0);
            scene.layout(root, 0, 4, 0, 0, 4.0, 0);
            Some(scene)
        }
    }

    pub fn root(&self) -> OtHandle {
        self.root
    }

    pub fn reset(&mut self, width: u32, height: u32) -> bool {
        let width = width.max(1);
        let height = height.clamp(1, MAX_CAPTURE_HEIGHT);
        if width != self.width || height != self.height {
            let status = unsafe {
                (self.api.session_resize_renderer)(self.context, &self.session, width, height)
            };
            if status != OT_OK {
                debug_status("ot_session_resize_renderer", status);
                return false;
            }
            self.width = width;
            self.height = height;
        }
        unsafe {
            (self.api.scene_destroy_node)(self.context, &self.root);
        }
        let mut root = OtHandle::default();
        let status = unsafe {
            (self.api.scene_create_node)(self.context, &self.session, OT_SCENE_ROOT, 1, &mut root)
        };
        if status != OT_OK {
            debug_status("ot_scene_create_node(root)", status);
            return false;
        }
        self.root = root;
        self.next_num = 2;
        self.nodes.clear();
        self.nodes.push(root);
        self.layout(root, 2, 0, 0, 2, 100.0, 0);
        self.layout(root, 2, 1, 0, 2, 100.0, 0);
        self.layout(root, 0, 4, 0, 0, 4.0, 0);
        true
    }

    pub fn create(&mut self, tag: u32) -> Option<OtHandle> {
        let num = self.next_num;
        self.next_num += 1;
        let mut handle = OtHandle::default();
        let status = unsafe {
            (self.api.scene_create_node)(self.context, &self.session, tag, num, &mut handle)
        };
        if status == OT_OK {
            self.nodes.push(handle);
            Some(handle)
        } else {
            None
        }
    }

    pub fn insert(&mut self, parent: OtHandle, child: OtHandle, index: u32) -> bool {
        unsafe { (self.api.scene_move_node)(self.context, &child, &parent, index) == OT_OK }
    }

    pub fn layout(
        &mut self,
        node: OtHandle,
        group: u32,
        kind: u32,
        edge: u32,
        unit: u32,
        value: f32,
        flags: u32,
    ) -> bool {
        unsafe {
            (self.api.scene_set_style)(self.context, &node, group, kind, edge, unit, value, flags)
                == OT_OK
        }
    }

    pub fn paint(&mut self, node: OtHandle, paint: &PaintOptions) -> bool {
        unsafe { (self.api.scene_set_paint)(self.context, &node, paint) == OT_OK }
    }

    pub fn text(&mut self, node: OtHandle, text: &str) -> bool {
        let bytes = text.as_bytes();
        unsafe {
            (self.api.scene_set_text)(
                self.context,
                &node,
                if bytes.is_empty() {
                    std::ptr::null()
                } else {
                    bytes.as_ptr()
                },
                bytes.len() as u32,
            ) == OT_OK
        }
    }

    pub fn styled_text(&mut self, node: OtHandle, chunks: &[StyledChunk<'_>]) -> bool {
        let mut bytes = Vec::new();
        let mut records = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            if chunk.text.is_empty() {
                continue;
            }
            let start = bytes.len() as u32;
            bytes.extend_from_slice(chunk.text.as_bytes());
            let mut flags = 0;
            let mut foreground = [0u16; 4];
            let mut background = [0u16; 4];
            if let Some(color) = chunk.fg {
                flags |= 1;
                foreground = color_words(color);
            }
            if let Some(color) = chunk.bg {
                flags |= 2;
                background = color_words(color);
            }
            records.push(StyledTextChunk {
                struct_size: std::mem::size_of::<StyledTextChunk>() as u32,
                abi_version: ABI_VERSION,
                byte_count: bytes.len() as u32 - start,
                flags,
                foreground,
                background,
                attributes: chunk.attributes,
                reserved: 0,
                link_offset: 0,
                link_byte_count: 0,
            });
        }
        if bytes.is_empty() {
            return self.text(node, "");
        }
        unsafe {
            (self.api.scene_set_styled_text)(
                self.context,
                &node,
                bytes.as_ptr(),
                bytes.len() as u32,
                if records.is_empty() {
                    std::ptr::null()
                } else {
                    records.as_ptr()
                },
                records.len() as u32,
                std::ptr::null(),
                0,
            ) == OT_OK
        }
    }

    pub fn text_options(&mut self, node: OtHandle, options: &TextOptions) -> bool {
        unsafe { (self.api.scene_set_text_options)(self.context, &node, options) == OT_OK }
    }

    pub fn set_lines(
        &mut self,
        node: OtHandle,
        lines: &[Line<'static>],
        fg: Option<Rgba>,
        bg: Option<Rgba>,
    ) -> bool {
        let mut chunks: Vec<StyledChunk<'_>> = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                chunks.push(StyledChunk {
                    text: "\n",
                    fg: None,
                    bg: None,
                    attributes: 0,
                });
            }
            for span in &line.spans {
                if span.content.is_empty() {
                    continue;
                }
                chunks.push(StyledChunk {
                    text: span.content.as_ref(),
                    fg: span.style.fg.and_then(color_to_rgba),
                    bg: span.style.bg.and_then(color_to_rgba),
                    attributes: modifier_bits(span.style.add_modifier),
                });
            }
        }
        let options = text_options_with(fg, bg, WRAP_NONE, TEXT_ALIGN_LEFT, true);
        self.text_options(node, &options) && self.styled_text(node, &chunks)
    }

    pub fn box_details(
        &mut self,
        node: OtHandle,
        title: &str,
        align: u32,
        color: Option<Rgba>,
    ) -> bool {
        let mut details = BoxDetails {
            struct_size: std::mem::size_of::<BoxDetails>() as u32,
            abi_version: ABI_VERSION,
            title_alignment: align,
            bottom_title_alignment: OT_SCENE_ALIGN_LEFT,
            flags: 0,
            reserved: 0,
            title_color: [0; 4],
            border_characters: [0; 11],
        };
        if let Some(color) = color {
            details.flags |= 1;
            details.title_color = color_words(color);
        }
        let title = title.as_bytes();
        unsafe {
            (self.api.scene_set_box_details)(
                self.context,
                &node,
                &details,
                if title.is_empty() {
                    std::ptr::null()
                } else {
                    title.as_ptr()
                },
                title.len() as u32,
                std::ptr::null(),
                0,
            ) == OT_OK
        }
    }

    pub fn measure(&mut self) -> bool {
        unsafe { (self.api.scene_measure_layout)(self.context, &self.session, &self.root) == OT_OK }
    }

    pub fn layout_rect(&self, node: OtHandle) -> Option<(f64, f64, f32, f32)> {
        let mut layout = SceneLayout {
            struct_size: std::mem::size_of::<SceneLayout>() as u32,
            abi_version: ABI_VERSION,
            ..Default::default()
        };
        let status = unsafe { (self.api.scene_get_layout)(self.context, &node, 2, &mut layout) };
        (status == OT_OK).then_some((
            layout.screen_x,
            layout.screen_y,
            layout.width,
            layout.height,
        ))
    }

    pub fn debug_rects(&self) -> Vec<(f64, f64, f32, f32)> {
        self.nodes
            .iter()
            .filter_map(|node| self.layout_rect(*node))
            .collect()
    }

    pub fn render(&mut self, background: Rgba) -> Option<Vec<Line<'static>>> {
        unsafe {
            let mut frame = FrameRequest::default();
            let background_words = color_words(background);
            let status = (self.api.scene_paint)(
                self.context,
                &self.session,
                background_words.as_ptr(),
                0,
                0,
                &mut frame,
            );
            if status != OT_OK {
                debug_status("ot_scene_paint", status);
                return None;
            }
            let mut snapshot = BufferLeaseSnapshot::default();
            let status = (self.api.frame_acquire_lease)(
                self.context,
                &self.session,
                &frame,
                OT_SESSION_BUFFER_NEXT,
                &mut snapshot,
            );
            if status != OT_OK {
                debug_status("ot_scene_frame_acquire_buffer_lease", status);
                (self.api.frame_cancel)(self.context, &self.session, frame.frame_id);
                return None;
            }
            let lines = self.capture(&snapshot);
            (self.api.lease_release)(self.context, &snapshot.lease);
            (self.api.frame_cancel)(self.context, &self.session, frame.frame_id);
            let mut lines = lines?;
            let blank = color_from_words(&color_words(background));
            while lines.len() > 1 && row_is_blank(lines.last().unwrap(), blank) {
                lines.pop();
            }
            Some(lines)
        }
    }

    unsafe fn capture(&self, snapshot: &BufferLeaseSnapshot) -> Option<Vec<Line<'static>>> {
        let cells = (snapshot.width as usize).checked_mul(snapshot.height as usize)?;
        if cells == 0
            || snapshot.char_ptr == 0
            || snapshot.fg_ptr == 0
            || snapshot.bg_ptr == 0
            || snapshot.attributes_ptr == 0
        {
            return None;
        }
        let mut size = 0u32;
        if (self.api.lease_char_size)(self.context, &snapshot.lease, 0, &mut size) != OT_OK {
            return None;
        }
        let mut bytes = vec![0u8; size as usize];
        let mut lengths = vec![0u8; cells];
        let mut written = 0u32;
        if (self.api.lease_write_chars)(
            self.context,
            &snapshot.lease,
            bytes.as_mut_ptr(),
            size,
            0,
            lengths.as_mut_ptr(),
            cells as u32,
            &mut written,
        ) != OT_OK
        {
            return None;
        }
        bytes.truncate(written as usize);
        let chars = snapshot.char_ptr as usize as *const u32;
        let fg = snapshot.fg_ptr as usize as *const u16;
        let bg = snapshot.bg_ptr as usize as *const u16;
        let attrs = snapshot.attributes_ptr as usize as *const u32;
        let mut offset = 0usize;
        let mut rows: Vec<Line<'static>> = Vec::with_capacity(snapshot.height as usize);
        for y in 0..snapshot.height as usize {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut text = String::new();
            let mut current = Style::default();
            for x in 0..snapshot.width as usize {
                let index = y * snapshot.width as usize + x;
                let code = *chars.add(index);
                if code & OT_CELL_CHAR_KIND_MASK == OT_CELL_CHAR_CONTINUATION {
                    continue;
                }
                let length = lengths[index] as usize;
                let end = (offset + length).min(bytes.len());
                let symbol = if end > offset {
                    String::from_utf8_lossy(&bytes[offset..end]).into_owned()
                } else {
                    " ".to_string()
                };
                offset = end;
                let style = Style::default()
                    .fg(color_from_words(&[
                        *fg.add(index * 4),
                        *fg.add(index * 4 + 1),
                        *fg.add(index * 4 + 2),
                        *fg.add(index * 4 + 3),
                    ]))
                    .bg(color_from_words(&[
                        *bg.add(index * 4),
                        *bg.add(index * 4 + 1),
                        *bg.add(index * 4 + 2),
                        *bg.add(index * 4 + 3),
                    ]))
                    .add_modifier(modifiers(*attrs.add(index)));
                if style != current && !text.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut text), current));
                }
                current = style;
                text.push_str(&symbol);
            }
            if !text.is_empty() {
                spans.push(Span::styled(text, current));
            }
            rows.push(Line::from(spans));
        }
        while rows.len() > 1 && row_is_blank(rows.last().unwrap(), Color::Reset) {
            rows.pop();
        }
        Some(rows)
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        unsafe {
            (self.api.scene_destroy_node)(self.context, &self.root);
            (self.api.session_destroy)(self.context, &self.session);
            (self.api.context_destroy)(self.context);
        }
    }
}

pub struct StyledChunk<'a> {
    pub text: &'a str,
    pub fg: Option<Rgba>,
    pub bg: Option<Rgba>,
    pub attributes: u32,
}

pub const TEXT_ALIGN_LEFT: u32 = OT_SCENE_ALIGN_LEFT;
pub const TEXT_ALIGN_CENTER: u32 = OT_SCENE_ALIGN_CENTER;
pub const TEXT_ALIGN_RIGHT: u32 = OT_SCENE_ALIGN_RIGHT;
pub const WRAP_NONE: u32 = OT_SCENE_WRAP_NONE;
pub const WRAP_CHAR: u32 = OT_SCENE_WRAP_CHAR;
pub const WRAP_WORD: u32 = OT_SCENE_WRAP_WORD;
pub const NODE_BOX: u32 = OT_SCENE_BOX;
pub const NODE_TEXT: u32 = OT_SCENE_TEXT;
pub const BORDER_SINGLE: u32 = OT_BORDER_SINGLE;
pub const BORDER_DOUBLE: u32 = OT_BORDER_DOUBLE;
pub const BORDER_ROUNDED: u32 = OT_BORDER_ROUNDED;
pub const BORDER_HEAVY: u32 = OT_BORDER_HEAVY;
pub const ATTR_BOLD: u32 = OT_ATTR_BOLD;
pub const ATTR_DIM: u32 = OT_ATTR_DIM;
pub const ATTR_ITALIC: u32 = OT_ATTR_ITALIC;
pub const ATTR_UNDERLINE: u32 = OT_ATTR_UNDERLINE;
pub const ATTR_INVERSE: u32 = OT_ATTR_INVERSE;
pub const ATTR_STRIKETHROUGH: u32 = OT_ATTR_STRIKETHROUGH;

pub fn paint_options(
    background: Option<Rgba>,
    border: Option<(u32, Option<Rgba>)>,
) -> PaintOptions {
    let mut paint = PaintOptions::default();
    if let Some(background) = background {
        paint.should_fill = 1;
        paint.background = color_words(background);
    }
    if let Some((style, color)) = border {
        paint.border_sides = OT_BORDER_ALL;
        paint.border_style = style;
        if let Some(color) = color {
            paint.border_color = color_words(color);
        }
    }
    paint
}

pub fn text_options(fg: Option<Rgba>, bg: Option<Rgba>, wrap: u32, align: u32) -> TextOptions {
    text_options_with(fg, bg, wrap, align, false)
}

pub fn text_options_with(
    fg: Option<Rgba>,
    bg: Option<Rgba>,
    wrap: u32,
    align: u32,
    truncate: bool,
) -> TextOptions {
    let mut options = TextOptions::default();
    if let Some(fg) = fg {
        options.foreground = color_words(fg);
    }
    if let Some(bg) = bg {
        options.background = color_words(bg);
    }
    options.wrap_mode = wrap;
    options.text_align = align;
    options.truncate = u32::from(truncate);
    options
}

pub fn color_to_rgba(color: Color) -> Option<Rgba> {
    match color {
        Color::Reset => None,
        Color::Rgb(r, g, b) => Some(Rgba::from_ints(r, g, b, 255)),
        Color::Black => Some(Rgba::BLACK),
        Color::White => Some(Rgba::from_ints(255, 255, 255, 255)),
        Color::Red => Some(Rgba::from_ints(255, 0, 0, 255)),
        Color::Green => Some(Rgba::from_ints(0, 255, 0, 255)),
        Color::Yellow => Some(Rgba::from_ints(255, 255, 0, 255)),
        Color::Blue => Some(Rgba::from_ints(0, 0, 255, 255)),
        Color::Magenta => Some(Rgba::from_ints(255, 0, 255, 255)),
        Color::Cyan => Some(Rgba::from_ints(0, 255, 255, 255)),
        Color::Gray | Color::DarkGray => Some(Rgba::from_ints(128, 128, 128, 255)),
        Color::LightRed => Some(Rgba::from_ints(255, 102, 102, 255)),
        Color::LightGreen => Some(Rgba::from_ints(102, 255, 102, 255)),
        Color::LightYellow => Some(Rgba::from_ints(255, 255, 102, 255)),
        Color::LightBlue => Some(Rgba::from_ints(102, 102, 255, 255)),
        Color::LightMagenta => Some(Rgba::from_ints(255, 102, 255, 255)),
        Color::LightCyan => Some(Rgba::from_ints(102, 255, 255, 255)),
        Color::Indexed(_) => None,
    }
}

pub fn modifier_bits(modifier: Modifier) -> u32 {
    let mut value = 0;
    if modifier.contains(Modifier::BOLD) {
        value |= OT_ATTR_BOLD;
    }
    if modifier.contains(Modifier::DIM) {
        value |= OT_ATTR_DIM;
    }
    if modifier.contains(Modifier::ITALIC) {
        value |= OT_ATTR_ITALIC;
    }
    if modifier.contains(Modifier::UNDERLINED) {
        value |= OT_ATTR_UNDERLINE;
    }
    if modifier.contains(Modifier::REVERSED) {
        value |= OT_ATTR_INVERSE;
    }
    if modifier.contains(Modifier::CROSSED_OUT) {
        value |= OT_ATTR_STRIKETHROUGH;
    }
    value
}

pub fn color_words(color: Rgba) -> [u16; 4] {
    [
        (color.r.clamp(0.0, 1.0) * 255.0).round() as u16,
        (color.g.clamp(0.0, 1.0) * 255.0).round() as u16,
        (color.b.clamp(0.0, 1.0) * 255.0).round() as u16,
        (color.a.clamp(0.0, 1.0) * 255.0).round() as u16,
    ]
}

fn color_from_words(words: &[u16; 4]) -> Color {
    if words[3] == 0 {
        return Color::Reset;
    }
    Color::Rgb(
        (words[0] & 255) as u8,
        (words[1] & 255) as u8,
        (words[2] & 255) as u8,
    )
}

fn modifiers(attributes: u32) -> Modifier {
    let mut modifier = Modifier::empty();
    if attributes & OT_ATTR_BOLD != 0 {
        modifier |= Modifier::BOLD;
    }
    if attributes & OT_ATTR_DIM != 0 {
        modifier |= Modifier::DIM;
    }
    if attributes & OT_ATTR_ITALIC != 0 {
        modifier |= Modifier::ITALIC;
    }
    if attributes & OT_ATTR_UNDERLINE != 0 {
        modifier |= Modifier::UNDERLINED;
    }
    if attributes & OT_ATTR_INVERSE != 0 {
        modifier |= Modifier::REVERSED;
    }
    if attributes & OT_ATTR_STRIKETHROUGH != 0 {
        modifier |= Modifier::CROSSED_OUT;
    }
    modifier
}

fn row_is_blank(line: &Line<'static>, background: Color) -> bool {
    line.spans.iter().all(|span| {
        span.content.chars().all(|ch| ch == ' ')
            && match span.style.bg {
                None | Some(Color::Reset) => true,
                Some(color) => color == background,
            }
    })
}
