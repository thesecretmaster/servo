/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use crate::compositor_thread::CompositorReceiver;
use crate::compositor_thread::{
    InitialCompositorState, Msg, WebrenderCanvasMsg, WebrenderFontMsg, WebrenderMsg,
};
#[cfg(feature = "gl")]
use crate::gl;
use crate::touch::{TouchAction, TouchHandler};
use crate::windowing::{
    self, EmbedderCoordinates, MouseWindowEvent, WebRenderDebugOption, WindowMethods,
};
use crate::{CompositionPipeline, ConstellationMsg, SendableFrameTree};
use canvas::canvas_paint_thread::ImageUpdate;
use crossbeam_channel::Sender;
use embedder_traits::Cursor;
use euclid::{Point2D, Rect, Scale, Vector2D};
use fnv::{FnvHashMap, FnvHashSet};
use gfx_traits::{Epoch, FontData};
#[cfg(feature = "gl")]
use image::{DynamicImage, ImageFormat};
use ipc_channel::ipc;
use libc::c_void;
use log::warn;
use msg::constellation_msg::{
    PipelineId, PipelineIndex, PipelineNamespaceId, TopLevelBrowsingContextId,
};
use net_traits::image::base::Image;
use net_traits::image_cache::CorsStatus;
#[cfg(feature = "gl")]
use pixels::PixelFormat;
use profile_traits::time::{self as profile_time, profile, ProfilerCategory};
use script_traits::compositor::{HitTestInfo, ScrollTree};
use script_traits::CompositorEvent::{MouseButtonEvent, MouseMoveEvent, TouchEvent, WheelEvent};
use script_traits::{
    AnimationState, AnimationTickType, CompositorHitTestResult, LayoutControlMsg, MouseButton,
    MouseEventType, ScrollState, TouchEventType, TouchId, UntrustedNodeAddress, WheelDelta,
    WindowSizeData, WindowSizeType,
};
use servo_geometry::{DeviceIndependentPixel, FramebufferUintLength};
use std::collections::HashMap;
use std::env;
use std::fs::{create_dir_all, File};
use std::io::Write;
use std::num::NonZeroU32;
use std::rc::Rc;
use style_traits::{CSSPixel, DevicePixel, PinchZoomFactor};
use time::{now, precise_time_ns, precise_time_s};
use webrender_api::units::{
    DeviceIntPoint, DeviceIntSize, DevicePoint, LayoutPoint, LayoutVector2D, WorldPoint,
};
use webrender_api::{
    self, BuiltDisplayList, CaptureBits, DirtyRect, DocumentId, Epoch as WebRenderEpoch,
    ExternalScrollId, HitTestFlags, PipelineId as WebRenderPipelineId, RenderApi, ScrollClamping,
    ScrollLocation, Transaction, ZoomFactor,
};
use webrender_surfman::WebrenderSurfman;

#[derive(Debug, PartialEq)]
enum UnableToComposite {
    NotReadyToPaintImage(NotReadyToPaint),
}

#[derive(Debug, PartialEq)]
enum NotReadyToPaint {
    AnimationsActive,
    JustNotifiedConstellation,
    WaitingOnConstellation,
}

// Default viewport constraints
const MAX_ZOOM: f32 = 8.0;
const MIN_ZOOM: f32 = 0.1;

trait ConvertPipelineIdFromWebRender {
    fn from_webrender(&self) -> PipelineId;
}

impl ConvertPipelineIdFromWebRender for WebRenderPipelineId {
    fn from_webrender(&self) -> PipelineId {
        PipelineId {
            namespace_id: PipelineNamespaceId(self.0),
            index: PipelineIndex(NonZeroU32::new(self.1).expect("Webrender pipeline zero?")),
        }
    }
}

/// Holds the state when running reftests that determines when it is
/// safe to save the output image.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ReadyState {
    Unknown,
    WaitingForConstellationReply,
    ReadyToSaveImage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameTreeId(u32);

impl FrameTreeId {
    pub fn next(&mut self) {
        self.0 += 1;
    }
}

/// One pixel in layer coordinate space.
///
/// This unit corresponds to a "pixel" in layer coordinate space, which after scaling and
/// transformation becomes a device pixel.
#[derive(Clone, Copy, Debug)]
enum LayerPixel {}

struct RootPipeline {
    top_level_browsing_context_id: TopLevelBrowsingContextId,
    id: Option<PipelineId>,
}

/// NB: Never block on the constellation, because sometimes the constellation blocks on us.
pub struct IOCompositor<Window: WindowMethods + ?Sized> {
    /// The application window.
    pub window: Rc<Window>,

    /// The port on which we receive messages.
    port: CompositorReceiver,

    /// The root pipeline.
    root_pipeline: RootPipeline,

    /// Tracks details about each active pipeline that the compositor knows about.
    pipeline_details: HashMap<PipelineId, PipelineDetails>,

    /// The scene scale, to allow for zooming and high-resolution painting.
    scale: Scale<f32, LayerPixel, DevicePixel>,

    /// "Mobile-style" zoom that does not reflow the page.
    viewport_zoom: PinchZoomFactor,

    /// Viewport zoom constraints provided by @viewport.
    min_viewport_zoom: Option<PinchZoomFactor>,
    max_viewport_zoom: Option<PinchZoomFactor>,

    /// "Desktop-style" zoom that resizes the viewport to fit the window.
    page_zoom: Scale<f32, CSSPixel, DeviceIndependentPixel>,

    /// The type of composition to perform
    composite_target: CompositeTarget,

    /// Tracks whether we should composite this frame.
    composition_request: CompositionRequest,

    /// Tracks whether we are in the process of shutting down, or have shut down and should close
    /// the compositor.
    pub shutdown_state: ShutdownState,

    /// Tracks whether the zoom action has happened recently.
    zoom_action: bool,

    /// The time of the last zoom action has started.
    zoom_time: f64,

    /// The current frame tree ID (used to reject old paint buffers)
    frame_tree_id: FrameTreeId,

    /// The channel on which messages can be sent to the constellation.
    constellation_chan: Sender<ConstellationMsg>,

    /// The channel on which messages can be sent to the time profiler.
    time_profiler_chan: profile_time::ProfilerChan,

    /// Touch input state machine
    touch_handler: TouchHandler,

    /// Pending scroll/zoom events.
    pending_scroll_zoom_events: Vec<ScrollZoomEvent>,

    /// Whether we're waiting on a recomposite after dispatching a scroll.
    waiting_for_results_of_scroll: bool,

    /// Used by the logic that determines when it is safe to output an
    /// image for the reftest framework.
    ready_to_save_state: ReadyState,

    /// The webrender renderer.
    webrender: webrender::Renderer,

    /// The active webrender document.
    webrender_document: DocumentId,

    /// The webrender interface, if enabled.
    webrender_api: RenderApi,

    /// The surfman instance that webrender targets
    webrender_surfman: WebrenderSurfman,

    /// The GL bindings for webrender
    webrender_gl: Rc<dyn gleam::gl::Gl>,

    /// Some XR devices want to run on the main thread.
    pub webxr_main_thread: webxr::MainThreadRegistry,

    /// Map of the pending paint metrics per layout thread.
    /// The layout thread for each specific pipeline expects the compositor to
    /// paint frames with specific given IDs (epoch). Once the compositor paints
    /// these frames, it records the paint time for each of them and sends the
    /// metric to the corresponding layout thread.
    pending_paint_metrics: HashMap<PipelineId, Epoch>,

    /// The coordinates of the native window, its view and the screen.
    embedder_coordinates: EmbedderCoordinates,

    /// Current mouse cursor.
    cursor: Cursor,

    /// Current cursor position.
    cursor_pos: DevicePoint,

    output_file: Option<String>,

    is_running_problem_test: bool,

    /// True to exit after page load ('-x').
    exit_after_load: bool,

    /// True to translate mouse input into touch events.
    convert_mouse_to_touch: bool,

    /// True if a WR frame render has been requested. Screenshots
    /// taken before the render is complete will not reflect the
    /// most up to date rendering.
    waiting_on_pending_frame: bool,
}

#[derive(Clone, Copy)]
struct ScrollZoomEvent {
    /// Change the pinch zoom level by this factor
    magnification: f32,
    /// Scroll by this offset, or to Start or End
    scroll_location: ScrollLocation,
    /// Apply changes to the frame at this location
    cursor: DeviceIntPoint,
    /// The number of OS events that have been coalesced together into this one event.
    event_count: u32,
}

#[derive(Debug, PartialEq)]
enum CompositionRequest {
    NoCompositingNecessary,
    CompositeNow(CompositingReason),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShutdownState {
    NotShuttingDown,
    ShuttingDown,
    FinishedShuttingDown,
}

struct PipelineDetails {
    /// The pipeline associated with this PipelineDetails object.
    pipeline: Option<CompositionPipeline>,

    /// Whether animations are running
    animations_running: bool,

    /// Whether there are animation callbacks
    animation_callbacks_running: bool,

    /// Whether this pipeline is visible
    visible: bool,

    /// Hit test items for this pipeline. This is used to map WebRender hit test
    /// information to the full information necessary for Servo.
    hit_test_items: Vec<HitTestInfo>,

    /// The compositor-side [ScrollTree]. This is used to allow finding and scrolling
    /// nodes in the compositor before forwarding new offsets to WebRender.
    scroll_tree: ScrollTree,
}

impl PipelineDetails {
    fn new() -> PipelineDetails {
        PipelineDetails {
            pipeline: None,
            animations_running: false,
            animation_callbacks_running: false,
            visible: true,
            hit_test_items: Vec::new(),
            scroll_tree: ScrollTree::default(),
        }
    }

    fn install_new_scroll_tree(&mut self, new_scroll_tree: ScrollTree) {
        let old_scroll_offsets: FnvHashMap<ExternalScrollId, LayoutVector2D> = self
            .scroll_tree
            .nodes
            .drain(..)
            .filter_map(|node| match (node.external_id(), node.offset()) {
                (Some(external_id), Some(offset)) => Some((external_id, offset)),
                _ => None,
            })
            .collect();

        self.scroll_tree = new_scroll_tree;
        for node in self.scroll_tree.nodes.iter_mut() {
            match node.external_id() {
                Some(external_id) => match old_scroll_offsets.get(&external_id) {
                    Some(new_offset) => node.set_offset(*new_offset),
                    None => continue,
                },
                _ => continue,
            };
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CompositeTarget {
    /// Normal composition to a window
    Window,

    /// Compose as normal, but also return a PNG of the composed output
    WindowAndPng,

    /// Compose to a PNG, write it to disk, and then exit the browser (used for reftests)
    PngFile,
}

impl<Window: WindowMethods + ?Sized> IOCompositor<Window> {
    fn new(
        window: Rc<Window>,
        state: InitialCompositorState,
        output_file: Option<String>,
        is_running_problem_test: bool,
        exit_after_load: bool,
        convert_mouse_to_touch: bool,
        top_level_browsing_context_id: TopLevelBrowsingContextId,
    ) -> Self {
        let composite_target = match output_file {
            Some(_) => CompositeTarget::PngFile,
            None => CompositeTarget::Window,
        };

        IOCompositor {
            embedder_coordinates: window.get_coordinates(),
            window,
            port: state.receiver,
            root_pipeline: RootPipeline {
                top_level_browsing_context_id,
                id: None,
            },
            pipeline_details: HashMap::new(),
            scale: Scale::new(1.0),
            composition_request: CompositionRequest::NoCompositingNecessary,
            touch_handler: TouchHandler::new(),
            pending_scroll_zoom_events: Vec::new(),
            waiting_for_results_of_scroll: false,
            composite_target,
            shutdown_state: ShutdownState::NotShuttingDown,
            page_zoom: Scale::new(1.0),
            viewport_zoom: PinchZoomFactor::new(1.0),
            min_viewport_zoom: None,
            max_viewport_zoom: None,
            zoom_action: false,
            zoom_time: 0f64,
            frame_tree_id: FrameTreeId(0),
            constellation_chan: state.constellation_chan,
            time_profiler_chan: state.time_profiler_chan,
            ready_to_save_state: ReadyState::Unknown,
            webrender: state.webrender,
            webrender_document: state.webrender_document,
            webrender_api: state.webrender_api,
            webrender_surfman: state.webrender_surfman,
            webrender_gl: state.webrender_gl,
            webxr_main_thread: state.webxr_main_thread,
            pending_paint_metrics: HashMap::new(),
            cursor: Cursor::None,
            cursor_pos: DevicePoint::new(0.0, 0.0),
            output_file,
            is_running_problem_test,
            exit_after_load,
            convert_mouse_to_touch,
            waiting_on_pending_frame: false,
        }
    }

    pub fn create(
        window: Rc<Window>,
        state: InitialCompositorState,
        output_file: Option<String>,
        is_running_problem_test: bool,
        exit_after_load: bool,
        convert_mouse_to_touch: bool,
        top_level_browsing_context_id: TopLevelBrowsingContextId,
    ) -> Self {
        let mut compositor = IOCompositor::new(
            window,
            state,
            output_file,
            is_running_problem_test,
            exit_after_load,
            convert_mouse_to_touch,
            top_level_browsing_context_id,
        );

        // Make sure the GL state is OK
        compositor.assert_gl_framebuffer_complete();

        // Set the size of the root layer.
        compositor.update_zoom_transform();

        compositor
    }

    pub fn deinit(self) {
        if let Err(err) = self.webrender_surfman.make_gl_context_current() {
            warn!("Failed to make GL context current: {:?}", err);
        }
        self.webrender.deinit();
    }

    fn update_cursor(&mut self, result: CompositorHitTestResult) {
        let cursor = match result.cursor {
            Some(cursor) if cursor != self.cursor => cursor,
            _ => return,
        };

        self.cursor = cursor;
        let msg = ConstellationMsg::SetCursor(cursor);
        if let Err(e) = self.constellation_chan.send(msg) {
            warn!("Sending event to constellation failed ({:?}).", e);
        }
    }

    pub fn maybe_start_shutting_down(&mut self) {
        if self.shutdown_state == ShutdownState::NotShuttingDown {
            debug!("Shutting down the constellation for WindowEvent::Quit");
            self.start_shutting_down();
        }
    }

    fn start_shutting_down(&mut self) {
        debug!("Compositor sending Exit message to Constellation");
        if let Err(e) = self.constellation_chan.send(ConstellationMsg::Exit) {
            warn!("Sending exit message to constellation failed ({:?}).", e);
        }

        self.shutdown_state = ShutdownState::ShuttingDown;
    }

    fn finish_shutting_down(&mut self) {
        debug!("Compositor received message that constellation shutdown is complete");

        // Drain compositor port, sometimes messages contain channels that are blocking
        // another thread from finishing (i.e. SetFrameTree).
        while self.port.try_recv_compositor_msg().is_some() {}

        // Tell the profiler, memory profiler, and scrolling timer to shut down.
        if let Ok((sender, receiver)) = ipc::channel() {
            self.time_profiler_chan
                .send(profile_time::ProfilerMsg::Exit(sender));
            let _ = receiver.recv();
        }

        self.shutdown_state = ShutdownState::FinishedShuttingDown;
    }

    fn handle_browser_message(&mut self, msg: Msg) -> bool {
        match (msg, self.shutdown_state) {
            (_, ShutdownState::FinishedShuttingDown) => {
                error!("compositor shouldn't be handling messages after shutting down");
                return false;
            },

            (Msg::ShutdownComplete, _) => {
                self.finish_shutting_down();
                return false;
            },

            (
                Msg::ChangeRunningAnimationsState(pipeline_id, animation_state),
                ShutdownState::NotShuttingDown,
            ) => {
                self.change_running_animations_state(pipeline_id, animation_state);
            },

            (Msg::SetFrameTree(frame_tree), ShutdownState::NotShuttingDown) => {
                self.set_frame_tree(&frame_tree);
                self.send_scroll_positions_to_layout_for_pipeline(&frame_tree.pipeline.id);
            },

            (Msg::Recomposite(reason), ShutdownState::NotShuttingDown) => {
                self.waiting_on_pending_frame = false;
                self.composition_request = CompositionRequest::CompositeNow(reason)
            },

            (Msg::TouchEventProcessed(result), ShutdownState::NotShuttingDown) => {
                self.touch_handler.on_event_processed(result);
            },

            (Msg::CreatePng(rect, reply), ShutdownState::NotShuttingDown) => {
                let res = self.composite_specific_target(CompositeTarget::WindowAndPng, rect);
                if let Err(ref e) = res {
                    info!("Error retrieving PNG: {:?}", e);
                }
                let img = res.unwrap_or(None);
                if let Err(e) = reply.send(img) {
                    warn!("Sending reply to create png failed ({:?}).", e);
                }
            },

            (Msg::IsReadyToSaveImageReply(is_ready), ShutdownState::NotShuttingDown) => {
                assert_eq!(
                    self.ready_to_save_state,
                    ReadyState::WaitingForConstellationReply
                );
                if is_ready && !self.waiting_on_pending_frame && !self.waiting_for_results_of_scroll
                {
                    self.ready_to_save_state = ReadyState::ReadyToSaveImage;
                    if self.is_running_problem_test {
                        println!("ready to save image!");
                    }
                } else {
                    self.ready_to_save_state = ReadyState::Unknown;
                    if self.is_running_problem_test {
                        println!("resetting ready_to_save_state!");
                    }
                }
                self.composite_if_necessary(CompositingReason::Headless);
            },

            (
                Msg::PipelineVisibilityChanged(pipeline_id, visible),
                ShutdownState::NotShuttingDown,
            ) => {
                self.pipeline_details(pipeline_id).visible = visible;
                self.process_animations();
            },

            (Msg::PipelineExited(pipeline_id, sender), _) => {
                debug!("Compositor got pipeline exited: {:?}", pipeline_id);
                self.remove_pipeline_root_layer(pipeline_id);
                let _ = sender.send(());
            },

            (Msg::NewScrollFrameReady(recomposite_needed), ShutdownState::NotShuttingDown) => {
                self.waiting_for_results_of_scroll = false;
                if let Some(result) = self.hit_test_at_device_point(self.cursor_pos) {
                    self.update_cursor(result);
                }
                if recomposite_needed {
                    self.composition_request = CompositionRequest::CompositeNow(
                        CompositingReason::NewWebRenderScrollFrame,
                    );
                }
            },

            (Msg::Dispatch(func), ShutdownState::NotShuttingDown) => {
                // The functions sent here right now are really dumb, so they can't panic.
                // But if we start running more complex code here, we should really catch panic here.
                func();
            },

            (Msg::LoadComplete(_), ShutdownState::NotShuttingDown) => {
                // If we're painting in headless mode, schedule a recomposite.
                if self.output_file.is_some() || self.exit_after_load {
                    self.composite_if_necessary(CompositingReason::Headless);
                }
            },

            (
                Msg::WebDriverMouseButtonEvent(mouse_event_type, mouse_button, x, y),
                ShutdownState::NotShuttingDown,
            ) => {
                let dppx = self.device_pixels_per_page_px();
                let point = dppx.transform_point(Point2D::new(x, y));
                self.on_mouse_window_event_class(match mouse_event_type {
                    MouseEventType::Click => MouseWindowEvent::Click(mouse_button, point),
                    MouseEventType::MouseDown => MouseWindowEvent::MouseDown(mouse_button, point),
                    MouseEventType::MouseUp => MouseWindowEvent::MouseUp(mouse_button, point),
                });
            },

            (Msg::WebDriverMouseMoveEvent(x, y), ShutdownState::NotShuttingDown) => {
                let dppx = self.device_pixels_per_page_px();
                let point = dppx.transform_point(Point2D::new(x, y));
                self.on_mouse_window_move_event_class(DevicePoint::new(point.x, point.y));
            },

            (Msg::PendingPaintMetric(pipeline_id, epoch), _) => {
                self.pending_paint_metrics.insert(pipeline_id, epoch);
            },

            (Msg::GetClientWindow(req), ShutdownState::NotShuttingDown) => {
                if let Err(e) = req.send(self.embedder_coordinates.window) {
                    warn!("Sending response to get client window failed ({:?}).", e);
                }
            },

            (Msg::GetScreenSize(req), ShutdownState::NotShuttingDown) => {
                if let Err(e) = req.send(self.embedder_coordinates.screen) {
                    warn!("Sending response to get screen size failed ({:?}).", e);
                }
            },

            (Msg::GetScreenAvailSize(req), ShutdownState::NotShuttingDown) => {
                if let Err(e) = req.send(self.embedder_coordinates.screen_avail) {
                    warn!(
                        "Sending response to get screen avail size failed ({:?}).",
                        e
                    );
                }
            },

            (Msg::Webrender(msg), ShutdownState::NotShuttingDown) => {
                self.handle_webrender_message(msg);
            },

            // When we are shutting_down, we need to avoid performing operations
            // such as Paint that may crash because we have begun tearing down
            // the rest of our resources.
            (_, ShutdownState::ShuttingDown) => {},
        }

        true
    }

    /// Accept messages from content processes that need to be relayed to the WebRender
    /// instance in the parent process.
    fn handle_webrender_message(&mut self, msg: WebrenderMsg) {
        match msg {
            WebrenderMsg::Layout(script_traits::WebrenderMsg::SendInitialTransaction(pipeline)) => {
                self.waiting_on_pending_frame = true;
                let mut txn = Transaction::new();
                txn.set_display_list(
                    WebRenderEpoch(0),
                    None,
                    Default::default(),
                    (pipeline, Default::default(), Default::default()),
                    false,
                );
                self.webrender_api
                    .send_transaction(self.webrender_document, txn);
            },

            WebrenderMsg::Layout(script_traits::WebrenderMsg::SendScrollNode(
                point,
                scroll_id,
                clamping,
            )) => {
                self.waiting_for_results_of_scroll = true;

                let mut txn = Transaction::new();
                txn.scroll_node_with_id(point, scroll_id, clamping);
                txn.generate_frame();
                self.webrender_api
                    .send_transaction(self.webrender_document, txn);
            },

            WebrenderMsg::Layout(script_traits::WebrenderMsg::SendDisplayList {
                display_list_info,
                content_size,
                display_list_descriptor,
                display_list_receiver,
            }) => match display_list_receiver.recv() {
                Ok(data) => {
                    self.waiting_on_pending_frame = true;

                    let pipeline_id = display_list_info.pipeline_id;
                    let details = self.pipeline_details(PipelineId::from_webrender(pipeline_id));
                    details.hit_test_items = display_list_info.hit_test_info;
                    details.install_new_scroll_tree(display_list_info.scroll_tree);

                    let mut txn = Transaction::new();
                    txn.set_display_list(
                        display_list_info.epoch,
                        None,
                        display_list_info.viewport_size,
                        (
                            pipeline_id,
                            content_size,
                            BuiltDisplayList::from_data(data, display_list_descriptor),
                        ),
                        true,
                    );
                    txn.generate_frame();
                    self.webrender_api
                        .send_transaction(self.webrender_document, txn);
                },
                Err(e) => warn!("error receiving display list data: {e:?}"),
            },

            WebrenderMsg::Layout(script_traits::WebrenderMsg::HitTest(
                pipeline,
                point,
                flags,
                sender,
            )) => {
                // When a display list is sent to WebRender, it starts scene building in a
                // separate thread and then that display list is available for hit testing.
                // Without flushing scene building, any hit test we do might be done against
                // a previous scene, if the last one we sent hasn't finished building.
                //
                // TODO(mrobinson): Flushing all scene building is a big hammer here, because
                // we might only be interested in a single pipeline. The only other option
                // would be to listen to the TransactionNotifier for previous per-pipeline
                // transactions, but that isn't easily compatible with the event loop wakeup
                // mechanism from libserver.
                self.webrender_api.flush_scene_builder();

                let result = self.hit_test_at_point_with_flags_and_pipeline(point, flags, pipeline);
                let _ = sender.send(result);
            },

            WebrenderMsg::Layout(script_traits::WebrenderMsg::GenerateImageKey(sender)) |
            WebrenderMsg::Net(net_traits::WebrenderImageMsg::GenerateImageKey(sender)) => {
                let _ = sender.send(self.webrender_api.generate_image_key());
            },

            WebrenderMsg::Layout(script_traits::WebrenderMsg::UpdateImages(updates)) => {
                let mut txn = Transaction::new();
                for update in updates {
                    match update {
                        script_traits::SerializedImageUpdate::AddImage(key, desc, data) => {
                            match data.to_image_data() {
                                Ok(data) => txn.add_image(key, desc, data, None),
                                Err(e) => warn!("error when sending image data: {:?}", e),
                            }
                        },
                        script_traits::SerializedImageUpdate::DeleteImage(key) => {
                            txn.delete_image(key)
                        },
                        script_traits::SerializedImageUpdate::UpdateImage(key, desc, data) => {
                            match data.to_image_data() {
                                Ok(data) => txn.update_image(key, desc, data, &DirtyRect::All),
                                Err(e) => warn!("error when sending image data: {:?}", e),
                            }
                        },
                    }
                }
                self.webrender_api
                    .send_transaction(self.webrender_document, txn);
            },

            WebrenderMsg::Net(net_traits::WebrenderImageMsg::AddImage(key, desc, data)) => {
                let mut txn = Transaction::new();
                txn.add_image(key, desc, data, None);
                self.webrender_api
                    .send_transaction(self.webrender_document, txn);
            },

            WebrenderMsg::Font(WebrenderFontMsg::AddFontInstance(font_key, size, sender)) => {
                let key = self.webrender_api.generate_font_instance_key();
                let mut txn = Transaction::new();
                txn.add_font_instance(key, font_key, size, None, None, Vec::new());
                self.webrender_api
                    .send_transaction(self.webrender_document, txn);
                let _ = sender.send(key);
            },

            WebrenderMsg::Font(WebrenderFontMsg::AddFont(data, sender)) => {
                let font_key = self.webrender_api.generate_font_key();
                let mut txn = Transaction::new();
                match data {
                    FontData::Raw(bytes) => txn.add_raw_font(font_key, bytes, 0),
                    FontData::Native(native_font) => txn.add_native_font(font_key, native_font),
                }
                self.webrender_api
                    .send_transaction(self.webrender_document, txn);
                let _ = sender.send(font_key);
            },

            WebrenderMsg::Canvas(WebrenderCanvasMsg::GenerateKey(sender)) => {
                let _ = sender.send(self.webrender_api.generate_image_key());
            },

            WebrenderMsg::Canvas(WebrenderCanvasMsg::UpdateImages(updates)) => {
                let mut txn = Transaction::new();
                for update in updates {
                    match update {
                        ImageUpdate::Add(key, descriptor, data) => {
                            txn.add_image(key, descriptor, data, None)
                        },
                        ImageUpdate::Update(key, descriptor, data) => {
                            txn.update_image(key, descriptor, data, &DirtyRect::All)
                        },
                        ImageUpdate::Delete(key) => txn.delete_image(key),
                    }
                }
                self.webrender_api
                    .send_transaction(self.webrender_document, txn);
            },
        }
    }

    /// Sets or unsets the animations-running flag for the given pipeline, and schedules a
    /// recomposite if necessary.
    fn change_running_animations_state(
        &mut self,
        pipeline_id: PipelineId,
        animation_state: AnimationState,
    ) {
        match animation_state {
            AnimationState::AnimationsPresent => {
                let visible = self.pipeline_details(pipeline_id).visible;
                self.pipeline_details(pipeline_id).animations_running = true;
                if visible {
                    self.composite_if_necessary(CompositingReason::Animation);
                }
            },
            AnimationState::AnimationCallbacksPresent => {
                let visible = self.pipeline_details(pipeline_id).visible;
                self.pipeline_details(pipeline_id)
                    .animation_callbacks_running = true;
                if visible {
                    self.tick_animations_for_pipeline(pipeline_id);
                }
            },
            AnimationState::NoAnimationsPresent => {
                self.pipeline_details(pipeline_id).animations_running = false;
            },
            AnimationState::NoAnimationCallbacksPresent => {
                self.pipeline_details(pipeline_id)
                    .animation_callbacks_running = false;
            },
        }
    }

    fn pipeline_details(&mut self, pipeline_id: PipelineId) -> &mut PipelineDetails {
        if !self.pipeline_details.contains_key(&pipeline_id) {
            self.pipeline_details
                .insert(pipeline_id, PipelineDetails::new());
        }
        self.pipeline_details
            .get_mut(&pipeline_id)
            .expect("Insert then get failed!")
    }

    pub fn pipeline(&self, pipeline_id: PipelineId) -> Option<&CompositionPipeline> {
        match self.pipeline_details.get(&pipeline_id) {
            Some(ref details) => details.pipeline.as_ref(),
            None => {
                warn!(
                    "Compositor layer has an unknown pipeline ({:?}).",
                    pipeline_id
                );
                None
            },
        }
    }

    fn set_frame_tree(&mut self, frame_tree: &SendableFrameTree) {
        debug!(
            "Setting the frame tree for pipeline {}",
            frame_tree.pipeline.id
        );

        self.root_pipeline = RootPipeline {
            top_level_browsing_context_id: frame_tree.pipeline.top_level_browsing_context_id,
            id: Some(frame_tree.pipeline.id),
        };

        let pipeline_id = frame_tree.pipeline.id.to_webrender();
        let mut txn = Transaction::new();
        txn.set_root_pipeline(pipeline_id);
        txn.generate_frame();
        self.webrender_api
            .send_transaction(self.webrender_document, txn);

        self.create_pipeline_details_for_frame_tree(&frame_tree);
        self.reset_scroll_tree_for_unattached_pipelines(&frame_tree);

        self.frame_tree_id.next();
    }

    fn reset_scroll_tree_for_unattached_pipelines(&mut self, frame_tree: &SendableFrameTree) {
        // TODO(mrobinson): Eventually this can selectively preserve the scroll trees
        // state for some unattached pipelines in order to preserve scroll position when
        // navigating backward and forward.
        fn collect_pipelines(
            pipelines: &mut FnvHashSet<PipelineId>,
            frame_tree: &SendableFrameTree,
        ) {
            pipelines.insert(frame_tree.pipeline.id);
            for kid in &frame_tree.children {
                collect_pipelines(pipelines, kid);
            }
        }

        let mut attached_pipelines: FnvHashSet<PipelineId> = FnvHashSet::default();
        collect_pipelines(&mut attached_pipelines, frame_tree);

        self.pipeline_details
            .iter_mut()
            .filter(|(id, _)| !attached_pipelines.contains(id))
            .for_each(|(_, details)| {
                details.scroll_tree.nodes.iter_mut().for_each(|node| {
                    node.set_offset(LayoutVector2D::zero());
                })
            })
    }

    fn create_pipeline_details_for_frame_tree(&mut self, frame_tree: &SendableFrameTree) {
        self.pipeline_details(frame_tree.pipeline.id).pipeline = Some(frame_tree.pipeline.clone());

        for kid in &frame_tree.children {
            self.create_pipeline_details_for_frame_tree(kid);
        }
    }

    fn remove_pipeline_root_layer(&mut self, pipeline_id: PipelineId) {
        self.pipeline_details.remove(&pipeline_id);
    }

    fn send_window_size(&self, size_type: WindowSizeType) {
        let dppx = self.page_zoom * self.embedder_coordinates.hidpi_factor;

        self.webrender_api.set_document_view(
            self.webrender_document,
            self.embedder_coordinates.get_flipped_viewport(),
            self.embedder_coordinates.hidpi_factor.get(),
        );

        let initial_viewport = self.embedder_coordinates.viewport.size.to_f32() / dppx;

        let data = WindowSizeData {
            device_pixel_ratio: dppx,
            initial_viewport: initial_viewport,
        };

        let top_level_browsing_context_id = self.root_pipeline.top_level_browsing_context_id;

        let msg = ConstellationMsg::WindowSize(top_level_browsing_context_id, data, size_type);

        if let Err(e) = self.constellation_chan.send(msg) {
            warn!("Sending window resize to constellation failed ({:?}).", e);
        }
    }

    pub fn on_resize_window_event(&mut self) -> bool {
        debug!("compositor resize requested");

        let old_coords = self.embedder_coordinates;
        self.embedder_coordinates = self.window.get_coordinates();

        // A size change could also mean a resolution change.
        if self.embedder_coordinates.hidpi_factor != old_coords.hidpi_factor {
            self.update_zoom_transform();
        }

        if self.embedder_coordinates.viewport == old_coords.viewport {
            return false;
        }

        self.send_window_size(WindowSizeType::Resize);
        self.composite_if_necessary(CompositingReason::Resize);
        return true;
    }

    pub fn on_mouse_window_event_class(&mut self, mouse_window_event: MouseWindowEvent) {
        if self.convert_mouse_to_touch {
            match mouse_window_event {
                MouseWindowEvent::Click(_, _) => {},
                MouseWindowEvent::MouseDown(_, p) => self.on_touch_down(TouchId(0), p),
                MouseWindowEvent::MouseUp(_, p) => self.on_touch_up(TouchId(0), p),
            }
            return;
        }

        self.dispatch_mouse_window_event_class(mouse_window_event);
    }

    fn dispatch_mouse_window_event_class(&mut self, mouse_window_event: MouseWindowEvent) {
        let point = match mouse_window_event {
            MouseWindowEvent::Click(_, p) => p,
            MouseWindowEvent::MouseDown(_, p) => p,
            MouseWindowEvent::MouseUp(_, p) => p,
        };

        let result = match self.hit_test_at_device_point(point) {
            Some(result) => result,
            None => return,
        };

        let (button, event_type) = match mouse_window_event {
            MouseWindowEvent::Click(button, _) => (button, MouseEventType::Click),
            MouseWindowEvent::MouseDown(button, _) => (button, MouseEventType::MouseDown),
            MouseWindowEvent::MouseUp(button, _) => (button, MouseEventType::MouseUp),
        };

        let event_to_send = MouseButtonEvent(
            event_type,
            button,
            result.point_in_viewport.to_untyped(),
            Some(result.node),
            Some(result.point_relative_to_item),
            button as u16,
        );

        let msg = ConstellationMsg::ForwardEvent(result.pipeline_id, event_to_send);
        if let Err(e) = self.constellation_chan.send(msg) {
            warn!("Sending event to constellation failed ({:?}).", e);
        }
    }

    fn hit_test_at_device_point(&self, point: DevicePoint) -> Option<CompositorHitTestResult> {
        let dppx = self.page_zoom * self.hidpi_factor();
        let scaled_point = (point / dppx).to_untyped();
        let world_point = WorldPoint::from_untyped(scaled_point);
        return self.hit_test_at_point(world_point);
    }

    fn hit_test_at_point(&self, point: WorldPoint) -> Option<CompositorHitTestResult> {
        return self
            .hit_test_at_point_with_flags_and_pipeline(point, HitTestFlags::empty(), None)
            .first()
            .cloned();
    }

    fn hit_test_at_point_with_flags_and_pipeline(
        &self,
        point: WorldPoint,
        flags: HitTestFlags,
        pipeline_id: Option<WebRenderPipelineId>,
    ) -> Vec<CompositorHitTestResult> {
        let root_pipeline_id = match self.root_pipeline.id {
            Some(root_pipeline_id) => root_pipeline_id,
            None => return vec![],
        };
        if self.pipeline(root_pipeline_id).is_none() {
            return vec![];
        }
        let results =
            self.webrender_api
                .hit_test(self.webrender_document, pipeline_id, point, flags);

        results
            .items
            .iter()
            .filter_map(|item| {
                let pipeline_id = PipelineId::from_webrender(item.pipeline);
                let details = match self.pipeline_details.get(&pipeline_id) {
                    Some(details) => details,
                    None => return None,
                };

                let info = &details.hit_test_items[item.tag.0 as usize];
                Some(CompositorHitTestResult {
                    pipeline_id,
                    point_in_viewport: item.point_in_viewport.to_untyped(),
                    point_relative_to_item: item.point_relative_to_item.to_untyped(),
                    node: UntrustedNodeAddress(info.node as *const c_void),
                    cursor: info.cursor,
                    scroll_tree_node: info.scroll_tree_node,
                })
            })
            .collect()
    }

    pub fn on_mouse_window_move_event_class(&mut self, cursor: DevicePoint) {
        if self.convert_mouse_to_touch {
            self.on_touch_move(TouchId(0), cursor);
            return;
        }

        self.dispatch_mouse_window_move_event_class(cursor);
    }

    fn dispatch_mouse_window_move_event_class(&mut self, cursor: DevicePoint) {
        let result = match self.hit_test_at_device_point(cursor) {
            Some(result) => result,
            None => return,
        };

        let event = MouseMoveEvent(result.point_in_viewport, Some(result.node), 0);
        let msg = ConstellationMsg::ForwardEvent(result.pipeline_id, event);
        if let Err(e) = self.constellation_chan.send(msg) {
            warn!("Sending event to constellation failed ({:?}).", e);
        }
        self.update_cursor(result);
    }

    fn send_touch_event(
        &self,
        event_type: TouchEventType,
        identifier: TouchId,
        point: DevicePoint,
    ) {
        if let Some(result) = self.hit_test_at_device_point(point) {
            let event = TouchEvent(
                event_type,
                identifier,
                result.point_in_viewport,
                Some(result.node),
            );
            let msg = ConstellationMsg::ForwardEvent(result.pipeline_id, event);
            if let Err(e) = self.constellation_chan.send(msg) {
                warn!("Sending event to constellation failed ({:?}).", e);
            }
        }
    }

    pub fn send_wheel_event(&mut self, delta: WheelDelta, point: DevicePoint) {
        if let Some(result) = self.hit_test_at_device_point(point) {
            let event = WheelEvent(delta, result.point_in_viewport, Some(result.node));
            let msg = ConstellationMsg::ForwardEvent(result.pipeline_id, event);
            if let Err(e) = self.constellation_chan.send(msg) {
                warn!("Sending event to constellation failed ({:?}).", e);
            }
        }
    }

    pub fn on_touch_event(
        &mut self,
        event_type: TouchEventType,
        identifier: TouchId,
        location: DevicePoint,
    ) {
        match event_type {
            TouchEventType::Down => self.on_touch_down(identifier, location),
            TouchEventType::Move => self.on_touch_move(identifier, location),
            TouchEventType::Up => self.on_touch_up(identifier, location),
            TouchEventType::Cancel => self.on_touch_cancel(identifier, location),
        }
    }

    fn on_touch_down(&mut self, identifier: TouchId, point: DevicePoint) {
        self.touch_handler.on_touch_down(identifier, point);
        self.send_touch_event(TouchEventType::Down, identifier, point);
    }

    fn on_touch_move(&mut self, identifier: TouchId, point: DevicePoint) {
        match self.touch_handler.on_touch_move(identifier, point) {
            TouchAction::Scroll(delta) => self.on_scroll_window_event(
                ScrollLocation::Delta(LayoutVector2D::from_untyped(delta.to_untyped())),
                point.cast(),
            ),
            TouchAction::Zoom(magnification, scroll_delta) => {
                let cursor = Point2D::new(-1, -1); // Make sure this hits the base layer.
                self.pending_scroll_zoom_events.push(ScrollZoomEvent {
                    magnification: magnification,
                    scroll_location: ScrollLocation::Delta(LayoutVector2D::from_untyped(
                        scroll_delta.to_untyped(),
                    )),
                    cursor: cursor,
                    event_count: 1,
                });
            },
            TouchAction::DispatchEvent => {
                self.send_touch_event(TouchEventType::Move, identifier, point);
            },
            _ => {},
        }
    }

    fn on_touch_up(&mut self, identifier: TouchId, point: DevicePoint) {
        self.send_touch_event(TouchEventType::Up, identifier, point);

        if let TouchAction::Click = self.touch_handler.on_touch_up(identifier, point) {
            self.simulate_mouse_click(point);
        }
    }

    fn on_touch_cancel(&mut self, identifier: TouchId, point: DevicePoint) {
        // Send the event to script.
        self.touch_handler.on_touch_cancel(identifier, point);
        self.send_touch_event(TouchEventType::Cancel, identifier, point);
    }

    /// <http://w3c.github.io/touch-events/#mouse-events>
    fn simulate_mouse_click(&mut self, p: DevicePoint) {
        let button = MouseButton::Left;
        self.dispatch_mouse_window_move_event_class(p);
        self.dispatch_mouse_window_event_class(MouseWindowEvent::MouseDown(button, p));
        self.dispatch_mouse_window_event_class(MouseWindowEvent::MouseUp(button, p));
        self.dispatch_mouse_window_event_class(MouseWindowEvent::Click(button, p));
    }

    pub fn on_wheel_event(&mut self, delta: WheelDelta, p: DevicePoint) {
        self.send_wheel_event(delta, p);
    }

    pub fn on_scroll_event(
        &mut self,
        delta: ScrollLocation,
        cursor: DeviceIntPoint,
        phase: TouchEventType,
    ) {
        match phase {
            TouchEventType::Move => self.on_scroll_window_event(delta, cursor),
            TouchEventType::Up | TouchEventType::Cancel => {
                self.on_scroll_window_event(delta, cursor);
            },
            TouchEventType::Down => {
                self.on_scroll_window_event(delta, cursor);
            },
        }
    }

    fn on_scroll_window_event(&mut self, scroll_location: ScrollLocation, cursor: DeviceIntPoint) {
        self.pending_scroll_zoom_events.push(ScrollZoomEvent {
            magnification: 1.0,
            scroll_location: scroll_location,
            cursor: cursor,
            event_count: 1,
        });
    }

    fn process_pending_scroll_events(&mut self) {
        // Batch up all scroll events into one, or else we'll do way too much painting.
        let mut last_combined_event: Option<ScrollZoomEvent> = None;
        for scroll_event in self.pending_scroll_zoom_events.drain(..) {
            let this_cursor = scroll_event.cursor;

            let this_delta = match scroll_event.scroll_location {
                ScrollLocation::Delta(delta) => delta,
                ScrollLocation::Start | ScrollLocation::End => {
                    // If this is an event which is scrolling to the start or end of the page,
                    // disregard other pending events and exit the loop.
                    last_combined_event = Some(scroll_event);
                    break;
                },
            };

            match &mut last_combined_event {
                last_combined_event @ &mut None => {
                    *last_combined_event = Some(ScrollZoomEvent {
                        magnification: scroll_event.magnification,
                        scroll_location: ScrollLocation::Delta(LayoutVector2D::from_untyped(
                            this_delta.to_untyped(),
                        )),
                        cursor: this_cursor,
                        event_count: 1,
                    })
                },
                &mut Some(ref mut last_combined_event) => {
                    // Mac OS X sometimes delivers scroll events out of vsync during a
                    // fling. This causes events to get bunched up occasionally, causing
                    // nasty-looking "pops". To mitigate this, during a fling we average
                    // deltas instead of summing them.
                    if let ScrollLocation::Delta(delta) = last_combined_event.scroll_location {
                        let old_event_count = Scale::new(last_combined_event.event_count as f32);
                        last_combined_event.event_count += 1;
                        let new_event_count = Scale::new(last_combined_event.event_count as f32);
                        last_combined_event.scroll_location = ScrollLocation::Delta(
                            (delta * old_event_count + this_delta) / new_event_count,
                        );
                    }
                    last_combined_event.magnification *= scroll_event.magnification;
                },
            }
        }

        if let Some(combined_event) = last_combined_event {
            let scroll_location = match combined_event.scroll_location {
                ScrollLocation::Delta(delta) => {
                    let scaled_delta =
                        (Vector2D::from_untyped(delta.to_untyped()) / self.scale).to_untyped();
                    let calculated_delta = LayoutVector2D::from_untyped(scaled_delta);
                    ScrollLocation::Delta(calculated_delta)
                },
                // Leave ScrollLocation unchanged if it is Start or End location.
                sl @ ScrollLocation::Start | sl @ ScrollLocation::End => sl,
            };
            let cursor = (combined_event.cursor.to_f32() / self.scale).to_untyped();
            let cursor = WorldPoint::from_untyped(cursor);
            let mut txn = Transaction::new();

            let result = match self.hit_test_at_point(cursor) {
                Some(result) => result,
                None => return,
            };

            if let Some(details) = self.pipeline_details.get_mut(&result.pipeline_id) {
                match details
                    .scroll_tree
                    .scroll_node_or_ancestor(&result.scroll_tree_node, scroll_location)
                {
                    Some((external_id, offset)) => {
                        let scroll_origin = LayoutPoint::new(-offset.x, -offset.y);
                        txn.scroll_node_with_id(
                            scroll_origin,
                            external_id,
                            ScrollClamping::NoClamping,
                        );
                    },
                    None => {},
                }
            }
            self.send_scroll_positions_to_layout_for_pipeline(&result.pipeline_id);

            if combined_event.magnification != 1.0 {
                let old_zoom = self.pinch_zoom_level();
                self.set_pinch_zoom_level(old_zoom * combined_event.magnification);
                txn.set_pinch_zoom(ZoomFactor::new(self.pinch_zoom_level()));
            }
            txn.generate_frame();
            self.webrender_api
                .send_transaction(self.webrender_document, txn);
            self.waiting_for_results_of_scroll = true
        }
    }

    /// If there are any animations running, dispatches appropriate messages to the constellation.
    fn process_animations(&mut self) {
        let mut pipeline_ids = vec![];
        for (pipeline_id, pipeline_details) in &self.pipeline_details {
            if (pipeline_details.animations_running || pipeline_details.animation_callbacks_running) &&
                pipeline_details.visible
            {
                pipeline_ids.push(*pipeline_id);
            }
        }
        let animation_state = if pipeline_ids.is_empty() && !self.webxr_main_thread.running() {
            windowing::AnimationState::Idle
        } else {
            windowing::AnimationState::Animating
        };
        self.window.set_animation_state(animation_state);
        for pipeline_id in &pipeline_ids {
            self.tick_animations_for_pipeline(*pipeline_id)
        }
    }

    fn tick_animations_for_pipeline(&mut self, pipeline_id: PipelineId) {
        let animation_callbacks_running = self
            .pipeline_details(pipeline_id)
            .animation_callbacks_running;
        let animations_running = self.pipeline_details(pipeline_id).animations_running;
        if !animation_callbacks_running && !animations_running {
            return;
        }

        let mut tick_type = AnimationTickType::empty();
        if animations_running {
            tick_type.insert(AnimationTickType::CSS_ANIMATIONS_AND_TRANSITIONS);
        }
        if animation_callbacks_running {
            tick_type.insert(AnimationTickType::REQUEST_ANIMATION_FRAME);
        }

        let msg = ConstellationMsg::TickAnimation(pipeline_id, tick_type);
        if let Err(e) = self.constellation_chan.send(msg) {
            warn!("Sending tick to constellation failed ({:?}).", e);
        }
    }

    fn hidpi_factor(&self) -> Scale<f32, DeviceIndependentPixel, DevicePixel> {
        if self.output_file.is_some() {
            return Scale::new(1.0);
        }
        self.embedder_coordinates.hidpi_factor
    }

    fn device_pixels_per_page_px(&self) -> Scale<f32, CSSPixel, DevicePixel> {
        self.page_zoom * self.hidpi_factor()
    }

    fn update_zoom_transform(&mut self) {
        let scale = self.device_pixels_per_page_px();
        self.scale = Scale::new(scale.get());
    }

    pub fn on_zoom_reset_window_event(&mut self) {
        self.page_zoom = Scale::new(1.0);
        self.update_zoom_transform();
        self.send_window_size(WindowSizeType::Resize);
        self.update_page_zoom_for_webrender();
    }

    pub fn on_zoom_window_event(&mut self, magnification: f32) {
        self.page_zoom = Scale::new(
            (self.page_zoom.get() * magnification)
                .max(MIN_ZOOM)
                .min(MAX_ZOOM),
        );
        self.update_zoom_transform();
        self.send_window_size(WindowSizeType::Resize);
        self.update_page_zoom_for_webrender();
    }

    fn update_page_zoom_for_webrender(&mut self) {
        let page_zoom = ZoomFactor::new(self.page_zoom.get());

        let mut txn = Transaction::new();
        txn.set_page_zoom(page_zoom);
        self.webrender_api
            .send_transaction(self.webrender_document, txn);
    }

    /// Simulate a pinch zoom
    pub fn on_pinch_zoom_window_event(&mut self, magnification: f32) {
        self.pending_scroll_zoom_events.push(ScrollZoomEvent {
            magnification: magnification,
            scroll_location: ScrollLocation::Delta(Vector2D::zero()), // TODO: Scroll to keep the center in view?
            cursor: Point2D::new(-1, -1), // Make sure this hits the base layer.
            event_count: 1,
        });
    }

    fn send_scroll_positions_to_layout_for_pipeline(&self, pipeline_id: &PipelineId) {
        let details = match self.pipeline_details.get(&pipeline_id) {
            Some(details) => details,
            None => return,
        };

        let mut scroll_states = Vec::new();
        details.scroll_tree.nodes.iter().for_each(|node| {
            match (node.external_id(), node.offset()) {
                (Some(scroll_id), Some(scroll_offset)) => {
                    scroll_states.push(ScrollState {
                        scroll_id,
                        scroll_offset,
                    });
                },
                _ => {},
            }
        });

        if let Some(pipeline) = details.pipeline.as_ref() {
            let _ = pipeline
                .layout_chan
                .send(LayoutControlMsg::SetScrollStates(scroll_states));
        }
    }

    // Check if any pipelines currently have active animations or animation callbacks.
    fn animations_active(&self) -> bool {
        for (_, details) in &self.pipeline_details {
            // If animations are currently running, then don't bother checking
            // with the constellation if the output image is stable.
            if details.animations_running {
                return true;
            }
            if details.animation_callbacks_running {
                return true;
            }
        }

        false
    }

    /// Query the constellation to see if the current compositor
    /// output matches the current frame tree output, and if the
    /// associated script threads are idle.
    fn is_ready_to_paint_image_output(&mut self) -> Result<(), NotReadyToPaint> {
        match self.ready_to_save_state {
            ReadyState::Unknown => {
                // Unsure if the output image is stable.

                // Collect the currently painted epoch of each pipeline that is
                // complete (i.e. has *all* layers painted to the requested epoch).
                // This gets sent to the constellation for comparison with the current
                // frame tree.
                let mut pipeline_epochs = HashMap::new();
                for (id, _) in &self.pipeline_details {
                    let webrender_pipeline_id = id.to_webrender();
                    if let Some(WebRenderEpoch(epoch)) = self
                        .webrender
                        .current_epoch(self.webrender_document, webrender_pipeline_id)
                    {
                        let epoch = Epoch(epoch);
                        pipeline_epochs.insert(*id, epoch);
                    }
                }

                // Pass the pipeline/epoch states to the constellation and check
                // if it's safe to output the image.
                let msg = ConstellationMsg::IsReadyToSaveImage(pipeline_epochs);
                if let Err(e) = self.constellation_chan.send(msg) {
                    warn!("Sending ready to save to constellation failed ({:?}).", e);
                }
                self.ready_to_save_state = ReadyState::WaitingForConstellationReply;
                Err(NotReadyToPaint::JustNotifiedConstellation)
            },
            ReadyState::WaitingForConstellationReply => {
                // If waiting on a reply from the constellation to the last
                // query if the image is stable, then assume not ready yet.
                Err(NotReadyToPaint::WaitingOnConstellation)
            },
            ReadyState::ReadyToSaveImage => {
                // Constellation has replied at some point in the past
                // that the current output image is stable and ready
                // for saving.
                // Reset the flag so that we check again in the future
                // TODO: only reset this if we load a new document?
                if self.is_running_problem_test {
                    println!("was ready to save, resetting ready_to_save_state");
                }
                self.ready_to_save_state = ReadyState::Unknown;
                Ok(())
            },
        }
    }

    pub fn composite(&mut self) {
        let target = self.composite_target;
        match self.composite_specific_target(target, None) {
            Ok(_) => {
                if self.output_file.is_some() || self.exit_after_load {
                    println!("Shutting down the Constellation after generating an output file or exit flag specified");
                    self.start_shutting_down();
                }
            },
            Err(e) => {
                if self.is_running_problem_test {
                    if e != UnableToComposite::NotReadyToPaintImage(
                        NotReadyToPaint::WaitingOnConstellation,
                    ) {
                        println!("not ready to composite: {:?}", e);
                    }
                }
            },
        }
    }

    /// Composite either to the screen or to a png image or both.
    /// Returns Ok if composition was performed or Err if it was not possible to composite
    /// for some reason. If CompositeTarget is Window or Png no image data is returned;
    /// in the latter case the image is written directly to a file. If CompositeTarget
    /// is WindowAndPng Ok(Some(png::Image)) is returned.
    fn composite_specific_target(
        &mut self,
        target: CompositeTarget,
        rect: Option<Rect<f32, CSSPixel>>,
    ) -> Result<Option<Image>, UnableToComposite> {
        let size = self.embedder_coordinates.framebuffer.to_u32();

        if let Err(err) = self.webrender_surfman.make_gl_context_current() {
            warn!("Failed to make GL context current: {:?}", err);
        }
        self.assert_no_gl_error();

        // Bind the webrender framebuffer
        let framebuffer_object = self
            .webrender_surfman
            .context_surface_info()
            .unwrap_or(None)
            .map(|info| info.framebuffer_object)
            .unwrap_or(0);
        self.webrender_gl
            .bind_framebuffer(gleam::gl::FRAMEBUFFER, framebuffer_object);
        self.assert_gl_framebuffer_complete();

        self.webrender.update();

        let wait_for_stable_image = match target {
            CompositeTarget::WindowAndPng | CompositeTarget::PngFile => true,
            CompositeTarget::Window => self.exit_after_load,
        };

        if wait_for_stable_image {
            // The current image may be ready to output. However, if there are animations active,
            // tick those instead and continue waiting for the image output to be stable AND
            // all active animations to complete.
            if self.animations_active() {
                self.process_animations();
                return Err(UnableToComposite::NotReadyToPaintImage(
                    NotReadyToPaint::AnimationsActive,
                ));
            }
            if let Err(result) = self.is_ready_to_paint_image_output() {
                return Err(UnableToComposite::NotReadyToPaintImage(result));
            }
        }

        let rt_info = match target {
            #[cfg(feature = "gl")]
            CompositeTarget::Window => gl::RenderTargetInfo::default(),
            #[cfg(feature = "gl")]
            CompositeTarget::WindowAndPng | CompositeTarget::PngFile => gl::initialize_png(
                &*self.webrender_gl,
                FramebufferUintLength::new(size.width),
                FramebufferUintLength::new(size.height),
            ),
            #[cfg(not(feature = "gl"))]
            _ => (),
        };

        profile(
            ProfilerCategory::Compositing,
            None,
            self.time_profiler_chan.clone(),
            || {
                debug!("compositor: compositing");

                let size =
                    DeviceIntSize::from_untyped(self.embedder_coordinates.framebuffer.to_untyped());

                // Paint the scene.
                // TODO(gw): Take notice of any errors the renderer returns!
                self.clear_background();
                self.webrender.render(size).ok();
            },
        );

        // If there are pending paint metrics, we check if any of the painted epochs is
        // one of the ones that the paint metrics recorder is expecting . In that case,
        // we get the current time, inform the layout thread about it and remove the
        // pending metric from the list.
        if !self.pending_paint_metrics.is_empty() {
            let paint_time = precise_time_ns();
            let mut to_remove = Vec::new();
            // For each pending paint metrics pipeline id
            for (id, pending_epoch) in &self.pending_paint_metrics {
                // we get the last painted frame id from webrender
                if let Some(WebRenderEpoch(epoch)) = self
                    .webrender
                    .current_epoch(self.webrender_document, id.to_webrender())
                {
                    // and check if it is the one the layout thread is expecting,
                    let epoch = Epoch(epoch);
                    if *pending_epoch != epoch {
                        continue;
                    }
                    // in which case, we remove it from the list of pending metrics,
                    to_remove.push(id.clone());
                    if let Some(pipeline) = self.pipeline(*id) {
                        // and inform the layout thread with the measured paint time.
                        let msg = LayoutControlMsg::PaintMetric(epoch, paint_time);
                        if let Err(e) = pipeline.layout_chan.send(msg) {
                            warn!("Sending PaintMetric message to layout failed ({:?}).", e);
                        }
                    }
                }
            }
            for id in to_remove.iter() {
                self.pending_paint_metrics.remove(id);
            }
        }

        let (x, y, width, height) = match rect {
            Some(rect) => {
                let rect = self.device_pixels_per_page_px().transform_rect(&rect);

                let x = rect.origin.x as i32;
                // We need to convert to the bottom-left origin coordinate
                // system used by OpenGL
                let y = (size.height as f32 - rect.origin.y - rect.size.height) as i32;
                let w = rect.size.width as u32;
                let h = rect.size.height as u32;

                (x, y, w, h)
            },
            None => (0, 0, size.width, size.height),
        };

        let rv = match target {
            CompositeTarget::Window => None,
            #[cfg(feature = "gl")]
            CompositeTarget::WindowAndPng => {
                let img = gl::draw_img(
                    &*self.webrender_gl,
                    rt_info,
                    x,
                    y,
                    FramebufferUintLength::new(width),
                    FramebufferUintLength::new(height),
                );
                Some(Image {
                    width: img.width(),
                    height: img.height(),
                    format: PixelFormat::RGB8,
                    bytes: ipc::IpcSharedMemory::from_bytes(&*img),
                    id: None,
                    cors_status: CorsStatus::Safe,
                })
            },
            #[cfg(feature = "gl")]
            CompositeTarget::PngFile => {
                let gl = &*self.webrender_gl;
                profile(
                    ProfilerCategory::ImageSaving,
                    None,
                    self.time_profiler_chan.clone(),
                    || match self.output_file.as_ref() {
                        Some(path) => match File::create(path) {
                            Ok(mut file) => {
                                let img = gl::draw_img(
                                    gl,
                                    rt_info,
                                    x,
                                    y,
                                    FramebufferUintLength::new(width),
                                    FramebufferUintLength::new(height),
                                );
                                let dynamic_image = DynamicImage::ImageRgb8(img);
                                if let Err(e) = dynamic_image.write_to(&mut file, ImageFormat::Png)
                                {
                                    error!("Failed to save {} ({}).", path, e);
                                }
                            },
                            Err(e) => error!("Failed to create {} ({}).", path, e),
                        },
                        None => error!("No file specified."),
                    },
                );
                None
            },
            #[cfg(not(feature = "gl"))]
            _ => None,
        };

        // Perform the page flip. This will likely block for a while.
        if let Err(err) = self.webrender_surfman.present() {
            warn!("Failed to present surface: {:?}", err);
        }

        self.composition_request = CompositionRequest::NoCompositingNecessary;

        self.process_animations();
        self.waiting_for_results_of_scroll = false;

        Ok(rv)
    }

    fn composite_if_necessary(&mut self, reason: CompositingReason) {
        if self.composition_request == CompositionRequest::NoCompositingNecessary {
            if self.is_running_problem_test {
                println!("updating composition_request ({:?})", reason);
            }
            self.composition_request = CompositionRequest::CompositeNow(reason)
        } else if self.is_running_problem_test {
            println!(
                "composition_request is already {:?}",
                self.composition_request
            );
        }
    }

    fn clear_background(&self) {
        let gl = &self.webrender_gl;
        self.assert_gl_framebuffer_complete();

        // Make framebuffer fully transparent.
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(gleam::gl::COLOR_BUFFER_BIT);
        self.assert_gl_framebuffer_complete();

        // Make the viewport white.
        let viewport = self.embedder_coordinates.get_flipped_viewport();
        gl.scissor(
            viewport.origin.x,
            viewport.origin.y,
            viewport.size.width,
            viewport.size.height,
        );
        gl.clear_color(1.0, 1.0, 1.0, 1.0);
        gl.enable(gleam::gl::SCISSOR_TEST);
        gl.clear(gleam::gl::COLOR_BUFFER_BIT);
        gl.disable(gleam::gl::SCISSOR_TEST);
        self.assert_gl_framebuffer_complete();
    }

    #[track_caller]
    fn assert_no_gl_error(&self) {
        debug_assert_eq!(self.webrender_gl.get_error(), gleam::gl::NO_ERROR);
    }

    #[track_caller]
    fn assert_gl_framebuffer_complete(&self) {
        debug_assert_eq!(
            (
                self.webrender_gl.get_error(),
                self.webrender_gl
                    .check_frame_buffer_status(gleam::gl::FRAMEBUFFER)
            ),
            (gleam::gl::NO_ERROR, gleam::gl::FRAMEBUFFER_COMPLETE)
        );
    }

    pub fn receive_messages(&mut self) -> bool {
        // Check for new messages coming from the other threads in the system.
        let mut compositor_messages = vec![];
        let mut found_recomposite_msg = false;
        while let Some(msg) = self.port.try_recv_compositor_msg() {
            match msg {
                Msg::Recomposite(_) if found_recomposite_msg => {},
                Msg::Recomposite(_) => {
                    found_recomposite_msg = true;
                    compositor_messages.push(msg)
                },
                _ => compositor_messages.push(msg),
            }
        }
        for msg in compositor_messages {
            if !self.handle_browser_message(msg) {
                return false;
            }
        }
        true
    }

    pub fn perform_updates(&mut self) -> bool {
        if self.shutdown_state == ShutdownState::FinishedShuttingDown {
            return false;
        }

        // If a pinch-zoom happened recently, ask for tiles at the new resolution
        if self.zoom_action && precise_time_s() - self.zoom_time > 0.3 {
            self.zoom_action = false;
        }

        match self.composition_request {
            CompositionRequest::NoCompositingNecessary => {},
            CompositionRequest::CompositeNow(_) => self.composite(),
        }

        // Run the WebXR main thread
        self.webxr_main_thread.run_one_frame();

        // The WebXR thread may make a different context current
        let _ = self.webrender_surfman.make_gl_context_current();

        if !self.pending_scroll_zoom_events.is_empty() && !self.waiting_for_results_of_scroll {
            self.process_pending_scroll_events()
        }
        self.shutdown_state != ShutdownState::FinishedShuttingDown
    }

    /// Repaints and recomposites synchronously. You must be careful when calling this, as if a
    /// paint is not scheduled the compositor will hang forever.
    ///
    /// This is used when resizing the window.
    pub fn repaint_synchronously(&mut self) {
        while self.shutdown_state != ShutdownState::ShuttingDown {
            let msg = self.port.recv_compositor_msg();
            let need_recomposite = match msg {
                Msg::Recomposite(_) => true,
                _ => false,
            };
            let keep_going = self.handle_browser_message(msg);
            if need_recomposite {
                self.composite();
                break;
            }
            if !keep_going {
                break;
            }
        }
    }

    pub fn pinch_zoom_level(&self) -> f32 {
        self.viewport_zoom.get()
    }

    fn set_pinch_zoom_level(&mut self, mut zoom: f32) {
        if let Some(min) = self.min_viewport_zoom {
            zoom = f32::max(min.get(), zoom);
        }
        if let Some(max) = self.max_viewport_zoom {
            zoom = f32::min(max.get(), zoom);
        }
        self.viewport_zoom = PinchZoomFactor::new(zoom);
    }

    pub fn toggle_webrender_debug(&mut self, option: WebRenderDebugOption) {
        let mut flags = self.webrender.get_debug_flags();
        let flag = match option {
            WebRenderDebugOption::Profiler => {
                webrender::DebugFlags::PROFILER_DBG |
                    webrender::DebugFlags::GPU_TIME_QUERIES |
                    webrender::DebugFlags::GPU_SAMPLE_QUERIES
            },
            WebRenderDebugOption::TextureCacheDebug => webrender::DebugFlags::TEXTURE_CACHE_DBG,
            WebRenderDebugOption::RenderTargetDebug => webrender::DebugFlags::RENDER_TARGET_DBG,
        };
        flags.toggle(flag);
        self.webrender.set_debug_flags(flags);

        let mut txn = Transaction::new();
        txn.generate_frame();
        self.webrender_api
            .send_transaction(self.webrender_document, txn);
    }

    pub fn capture_webrender(&mut self) {
        let capture_id = now().to_timespec().sec.to_string();
        let available_path = [env::current_dir(), Ok(env::temp_dir())]
            .iter()
            .filter_map(|val| {
                val.as_ref()
                    .map(|dir| dir.join("capture_webrender").join(&capture_id))
                    .ok()
            })
            .find(|val| match create_dir_all(&val) {
                Ok(_) => true,
                Err(err) => {
                    eprintln!("Unable to create path '{:?}' for capture: {:?}", &val, err);
                    false
                },
            });

        match available_path {
            Some(capture_path) => {
                let revision_file_path = capture_path.join("wr.txt");

                debug!(
                    "Trying to save webrender capture under {:?}",
                    &revision_file_path
                );
                self.webrender_api
                    .save_capture(capture_path, CaptureBits::all());

                match File::create(revision_file_path) {
                    Ok(mut file) => {
                        let revision = include!(concat!(env!("OUT_DIR"), "/webrender_revision.rs"));
                        if let Err(err) = write!(&mut file, "{}", revision) {
                            eprintln!("Unable to write webrender revision: {:?}", err)
                        }
                    },
                    Err(err) => eprintln!(
                        "Capture triggered, creating webrender revision info skipped: {:?}",
                        err
                    ),
                }
            },
            None => eprintln!("Unable to locate path to save captures"),
        }
    }
}

/// Why we performed a composite. This is used for debugging.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CompositingReason {
    /// We hit the delayed composition timeout. (See `delayed_composition.rs`.)
    DelayedCompositeTimeout,
    /// The window has been scrolled and we're starting the first recomposite.
    Scroll,
    /// A scroll has continued and we need to recomposite again.
    ContinueScroll,
    /// We're performing the single composite in headless mode.
    Headless,
    /// We're performing a composite to run an animation.
    Animation,
    /// A new frame tree has been loaded.
    NewFrameTree,
    /// New painted buffers have been received.
    NewPaintedBuffers,
    /// The window has been zoomed.
    Zoom,
    /// A new WebRender frame has arrived.
    NewWebRenderFrame,
    /// WebRender has processed a scroll event and has generated a new frame.
    NewWebRenderScrollFrame,
    /// The window has been resized and will need to be synchronously repainted.
    Resize,
}
