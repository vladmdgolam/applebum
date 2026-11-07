//! A Windows Media Player skin in the mini player's window.
//!
//! The skin runs in `skin::wmp::runtime`. This paints what it says to, in
//! its own pixels with nearest-neighbour scaling, and hands it the pointer
//! and keys as actions. Each view the skin opens is a window of its own: the
//! first in the mini player's window, the others in viewports of their own.

use std::time::Instant;

use egui::{
    Align2, Color32, FontId, Id, Pos2, Rect, ResizeDirection, Sense, Shape, Ui, ViewportBuilder,
    ViewportCommand, ViewportId, pos2, vec2,
};

use crate::app::App;
use crate::i18n::gettext;
use crate::model::Action;
use crate::settings::VisMode;
use crate::skin::wmp::Box2;
use crate::skin::wmp::live::{Edge, WindowCommand};
use crate::skin::wmp::paint::{Paint, TextPaint};
use crate::skin::wmp::runtime::{Input, Modifiers, Runtime};
use crate::vis;

use super::{menu, options_menu, unit};

/// A skin's window was asked to be this size last; a request a compositor
/// turned down is repeated at most once a second.
#[derive(Clone, Copy)]
struct Fit {
    wanted: egui::Vec2,
    at: f64,
}

fn fit_view(ctx: &egui::Context, wanted: egui::Vec2) {
    let current = ctx.viewport_rect().size();
    if (current - wanted).abs().max_elem() < 1.0 {
        return;
    }
    let id = Id::new("wmp-fit");
    let last: Option<Fit> = ctx.data(|data| data.get_temp(id));
    let now = ctx.input(|input| input.time);
    if last.is_some_and(|last| last.wanted == wanted && now - last.at < 1.0) {
        return;
    }
    ctx.data_mut(|data| data.insert_temp(id, Fit { wanted, at: now }));
    // Growing, the ceiling moves first; shrinking, the floor does. The other
    // order asks for a minimum above the maximum, which the window refuses
    // until something else makes it lay out again.
    if wanted.x >= current.x || wanted.y >= current.y {
        ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(wanted));
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(wanted));
    } else {
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(wanted));
        ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(wanted));
    }
    ctx.send_viewport_cmd(ViewportCommand::InnerSize(wanted));
    ctx.request_repaint();
}

/// Lets clicks through the skin's see-through parts to the windows beneath, as a
/// shaped window should. A window that passes the mouse through no longer hears
/// it, so the pointer is followed from the screen while it does.
#[cfg(target_os = "macos")]
fn pass_through_clear_parts(ctx: &egui::Context, runtime: &mut Runtime, key: u32, origin: Pos2, unit: f32) {
    use objc2_app_kit::{NSEvent, NSScreen};
    let id = Id::new(("wmp-pass-through", key));
    let passing: bool = ctx.data(|data| data.get_temp(id)).unwrap_or(false);
    let held = ctx.input(|input| input.pointer.any_down());
    let wanted = if held || egui::Popup::is_any_open(ctx) {
        false
    } else {
        let Some(inner) = ctx.input(|input| input.viewport().inner_rect) else {
            return;
        };
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            return;
        };
        let screens = NSScreen::screens(mtm);
        let Some(primary) = screens.iter().next() else {
            return;
        };
        // The screen counts up from the bottom; the window's rectangle from the top.
        let height = primary.frame().size.height;
        let pointer = NSEvent::mouseLocation();
        let at = egui::pos2(pointer.x as f32, (height - pointer.y) as f32);
        let local = at - inner.min - origin.to_vec2();
        let (x, y) = (f64::from(local.x / unit), f64::from(local.y / unit));
        inner.contains(at) && !runtime.solid_at(key, x, y)
    };
    if wanted != passing {
        ctx.data_mut(|data| data.insert_temp(id, wanted));
        ctx.send_viewport_cmd(ViewportCommand::MousePassthrough(wanted));
    }
    if wanted {
        ctx.request_repaint_after(std::time::Duration::from_millis(40));
    }
}

/// The whole mini player, with every window of the skin.
pub fn show(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let Some(mut runtime) = app.winamp.runtime.take() else {
        return;
    };
    let primary = runtime.primary().map(|at| runtime.views[at].state.key);
    if let Some(key) = primary {
        draw_view(app, &mut runtime, ui, key, true);
    }
    let others: Vec<(u32, (f64, f64), String)> = runtime
        .views
        .iter()
        .filter(|view| view.state.windowed() && Some(view.state.key) != primary)
        .map(|view| (view.state.key, view.state.size(), view.state.id.clone()))
        .collect();
    #[cfg(target_os = "linux")]
    {
        // A window the skin just opened has a border like any other.
        let id = Id::new("wmp-window-count");
        if ctx.data(|data| data.get_temp::<usize>(id)) != Some(others.len()) {
            ctx.data_mut(|data| data.insert_temp(id, others.len()));
            crate::hyprland::bare_skin_windows();
        }
    }
    let unit = unit(app, &ctx);
    for (key, (width, height), title) in others {
        let size = vec2(width as f32, height as f32) * unit;
        let builder = ViewportBuilder::default()
            .with_title(title)
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_inner_size(size)
            .with_taskbar(false);
        ctx.show_viewport_immediate(
            ViewportId::from_hash_of(("wmp-view", key)),
            builder,
            |ui, _class| {
                draw_view(app, &mut runtime, ui, key, false);
            },
        );
    }
    app.winamp.runtime = Some(runtime);
    if app.skin_lyrics_shown() {
        lyrics_strip(app, &ctx);
    }
}

/// Headspace's own greens: the lit lime of its frame, the shadow along its
/// edge, the dark screen of its drawers, and its playlist's two text colours.
const STRIP_LIME: egui::Color32 = egui::Color32::from_rgb(0xaa, 0xff, 0x2b);
const STRIP_MID: egui::Color32 = egui::Color32::from_rgb(0x63, 0xac, 0x05);
const STRIP_EDGE: egui::Color32 = egui::Color32::from_rgb(0x1d, 0x7c, 0x00);
const STRIP_SCREEN: egui::Color32 = egui::Color32::from_rgb(0x28, 0x5f, 0x03);
const STRIP_SUNG: egui::Color32 = egui::Color32::from_rgb(0x00, 0xff, 0x00);
const STRIP_TEXT: egui::Color32 = egui::Color32::from_rgb(0xc9, 0xd7, 0xbf);

/// The line being sung and its neighbours, in a strip that hangs under the
/// skin's window and follows it.
fn lyrics_strip(app: &mut App, ctx: &egui::Context) {
    let Some(player) = ctx.input(|input| input.viewport().outer_rect) else {
        return;
    };
    let width = (player.width() * 0.62).clamp(260.0, 520.0);
    let size = vec2(width, 112.0);
    let place = egui::pos2(player.center().x - width / 2.0, player.max.y + 6.0);
    let builder = ViewportBuilder::default()
        .with_title("Lyrics")
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_inner_size(size)
        .with_position(place)
        .with_taskbar(false);
    ctx.show_viewport_immediate(ViewportId::from_hash_of("applebum-lyrics-strip"), builder, |ui, _class| {
        let ctx = ui.ctx().clone();
        // Keep hanging under the player wherever it goes.
        if let Some(now) = ctx.input(|input| input.viewport().outer_rect)
            && (now.min - place).length() > 1.0
        {
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(place));
        }
        let rect = ui.max_rect();
        let painter = ui.painter();
        painter.rect_filled(rect, 18.0, STRIP_EDGE);
        painter.rect_filled(rect.shrink(2.0), 16.0, STRIP_MID);
        painter.rect_filled(
            Rect::from_min_max(rect.min + vec2(4.0, 3.0), egui::pos2(rect.max.x - 4.0, rect.min.y + 10.0)),
            8.0,
            STRIP_LIME.gamma_multiply(0.55),
        );
        let screen = rect.shrink2(vec2(11.0, 9.0));
        painter.rect_filled(screen, 10.0, STRIP_SCREEN);
        let clip = painter.with_clip_rect(screen.shrink(2.0));
        draw_strip_lines(app, &ctx, &clip, screen);
    });
}

fn draw_strip_lines(app: &App, ctx: &egui::Context, painter: &egui::Painter, screen: Rect) {
    let center = screen.center();
    let say = |text: &str| {
        painter.text(center, egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(13.0), STRIP_TEXT);
    };
    let Some(now) = app.now_playing() else {
        say("\u{266a}");
        return;
    };
    let lyrics = match &app.lyrics {
        crate::model::Loadable::Loaded(Some(lyrics)) if !lyrics.lines.is_empty() => lyrics,
        crate::model::Loadable::Loaded(Some(lyrics)) if lyrics.instrumental => {
            say("Instrumental");
            return;
        }
        crate::model::Loadable::Loaded(_) => {
            say("No lyrics for this song");
            return;
        }
        crate::model::Loadable::Failed(_) => {
            say("Couldn't fetch the lyrics");
            return;
        }
        _ => {
            say("\u{2026}");
            return;
        }
    };
    let spacing = 24.0;
    // Untimed lyrics just show their start; timed ones glide to the line sung.
    let active = if lyrics.synced { lyrics.active_line(now.position_ms) } else { None };
    let target = active.map_or(-0.6, |line| line as f32);
    let shown = ctx.animate_value_with_time(egui::Id::new("strip-line"), target, 0.28);
    if (shown - target).abs() > 0.001 {
        ctx.request_repaint();
    }
    let fit = screen.width() - 24.0;
    for (index, line) in lyrics.lines.iter().enumerate() {
        let y = center.y + (index as f32 - shown) * spacing;
        if y < screen.min.y - spacing || y > screen.max.y + spacing {
            continue;
        }
        let sung = active == Some(index);
        let distance = ((index as f32 - shown).abs() / 2.2).min(1.0);
        let (font, color) = if sung {
            (egui::FontId::proportional(16.0), STRIP_SUNG)
        } else {
            (egui::FontId::proportional(13.0), STRIP_TEXT.gamma_multiply(1.0 - distance * 0.65))
        };
        let text = if line.text.is_empty() { "\u{266a}" } else { line.text.as_str() };
        let galley = painter.layout(text.to_owned(), font, color, f32::INFINITY);
        // A long line shrinks to fit rather than running off the screen.
        let scale = (fit / galley.size().x).min(1.0);
        let galley = if scale < 1.0 {
            let font = egui::FontId::proportional(if sung { 16.0 } else { 13.0 } * scale.max(0.6));
            painter.layout(text.to_owned(), font, color, fit)
        } else {
            galley
        };
        let at = egui::pos2(center.x - galley.size().x / 2.0, y - galley.size().y / 2.0);
        painter.galley(at, galley, color);
    }
    // The next line is never more than a beat away: keep the clock moving.
    if lyrics.synced && now.playing {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

fn draw_view(app: &mut App, runtime: &mut Runtime, ui: &mut Ui, key: u32, primary: bool) {
    let ctx = ui.ctx().clone();
    let unit = unit(app, &ctx);
    let Some(view) = runtime.view(key) else {
        return;
    };
    let (width, height) = view.state.size();
    let resizing = view.state.resizing;
    let origin = ui.max_rect().min;
    let wanted = vec2(width as f32, height as f32) * unit;
    if resizing {
        let current = ctx.viewport_rect().size() / unit;
        if (current - wanted / unit).abs().max_elem() >= 1.0 {
            app.actions.push(Action::WmpInput {
                view: key,
                input: Input::Resized(current.x.round(), current.y.round()),
                modifiers: Modifiers::default(),
            });
        }
    } else {
        fit_view(&ctx, wanted);
    }
    if primary {
        // Off while typing: a skin's search box wants the space bar.
        let typing = runtime
            .view(key)
            .and_then(|view| view.state.focus.map(|id| &view.state.elements[id]))
            .is_some_and(|element| element.tag == "editbox");
        if !typing {
            super::super::keys::handle(app, &ctx);
        }
        for path in super::dropped_skins(&ctx) {
            app.actions.push(Action::InstallSkin(path));
        }
    }

    // Right-click for the menu; everything else is the skin's.
    let window = Rect::from_min_size(origin, wanted);
    let background = ui.interact(window, Id::new(("wmp-window", key)), Sense::click());
    let menu_scale = unit.max(2.0);
    let skin = app.winamp.skin.clone();
    menu(
        egui::Popup::context_menu(&background),
        &skin,
        menu_scale,
        |ui| {
            options_menu(app, ui, menu_scale);
        },
    );
    if !egui::Popup::is_any_open(&ctx) {
        read_input(app, &ctx, key, origin, unit);
    }

    let now = Instant::now();
    let (paints, wake) = runtime.paint(key, now);
    if (ctx.viewport_rect().size() - wanted).abs().max_elem() < 1.0 {
        runtime.window_ready(key);
    }
    let mut moving = false;
    for paint in &paints {
        moving |= draw(app, ui, paint, origin, unit, now);
    }
    app.winamp.trim_wmp_textures();

    for command in runtime.window_commands(key) {
        match command {
            WindowCommand::Drag => ctx.send_viewport_cmd(ViewportCommand::StartDrag),
            WindowCommand::Minimize => crate::window::minimize_window(&ctx),
            WindowCommand::Maximize => ctx.send_viewport_cmd(ViewportCommand::Maximized(true)),
            WindowCommand::Restore => ctx.send_viewport_cmd(ViewportCommand::Maximized(false)),
            WindowCommand::Resize(edge) => {
                let view = runtime.view(key).map(|view| {
                    let root = &view.state.elements[view.state.root];
                    (root.num("minwidth", 0.0), root.num("minheight", 0.0))
                });
                let (min_width, min_height) = view.unwrap_or_default();
                ctx.send_viewport_cmd(ViewportCommand::Resizable(true));
                ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(
                    vec2(min_width as f32, min_height as f32) * unit,
                ));
                ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(vec2(16384.0, 16384.0)));
                ctx.send_viewport_cmd(ViewportCommand::BeginResize(match edge {
                    Edge::North => ResizeDirection::North,
                    Edge::South => ResizeDirection::South,
                    Edge::East => ResizeDirection::East,
                    Edge::West => ResizeDirection::West,
                    Edge::NorthEast => ResizeDirection::NorthEast,
                    Edge::NorthWest => ResizeDirection::NorthWest,
                    Edge::SouthEast => ResizeDirection::SouthEast,
                    Edge::SouthWest => ResizeDirection::SouthWest,
                }));
            }
        }
    }

    let (tip, cursor) = runtime.hover_info(key);
    if let Some(cursor) = cursor
        && cursor.eq_ignore_ascii_case("hand")
    {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // At the pointer: the area hovered is the whole window, and a tooltip under
    // it would sit at the window's bottom corner.
    if let Some(tip) = tip {
        egui::Tooltip::for_widget(&background)
            .at_pointer()
            .gap(12.0)
            .show(|ui| {
                ui.label(tip);
            });
    }
    #[cfg(target_os = "macos")]
    pass_through_clear_parts(&ctx, runtime, key, origin, unit);

    if moving {
        crate::theme::visualiser_frame(&ctx, app.visualiser_fps());
    }
    if let Some(wake) = runtime.next_wake(now) {
        ctx.request_repaint_after(wake);
    }
    if let Some(wake) = wake {
        ctx.request_repaint_after(std::time::Duration::from_millis(wake));
    }
}

/// Where a pane's full screen button was last frame, in this window.
#[cfg(feature = "milkdrop")]
fn pane_button_id(ctx: &egui::Context) -> Id {
    Id::new(("wmp-pane-button", ctx.viewport_id()))
}

#[cfg(not(feature = "milkdrop"))]
fn pane_button_id(ctx: &egui::Context) -> Id {
    Id::new(ctx.viewport_id())
}

// The id is asked for outside the locks: using the context again from inside
// one freezes the app.
#[cfg(feature = "milkdrop")]
fn keep_pane_button(ctx: &egui::Context, button: Rect) {
    let id = pane_button_id(ctx);
    ctx.data_mut(|data| data.insert_temp(id, button));
}

fn take_pane_button(ctx: &egui::Context) -> Option<Rect> {
    let id = pane_button_id(ctx);
    ctx.data_mut(|data| {
        let button = data.get_temp::<Rect>(id);
        data.remove::<Rect>(id);
        button
    })
}

/// Whether a press lands on our button over a pane, and so is not the skin's.
/// With the press kept back, the skin takes the release for nothing.
fn on_our_button(button: Option<Rect>, at: Pos2) -> bool {
    button.is_some_and(|button| button.contains(at))
}

/// A pane's way to full screen: a double click on it, or the button that
/// shows while the pointer is over it.
#[cfg(feature = "milkdrop")]
fn pane_full_screen(
    app: &mut App,
    ui: &mut Ui,
    rect: Rect,
    clip: &[Box2],
    origin: Pos2,
    unit: f32,
) {
    /// Smaller panes keep their picture to themselves.
    const LEAST_PANE: f32 = 48.0;
    const SIDE: f32 = 22.0;
    let ctx = ui.ctx().clone();
    let inside = ctx
        .input(|input| input.pointer.latest_pos())
        .is_some_and(|at| {
            rect.contains(at)
                && clip
                    .iter()
                    .any(|box2| world(*box2, origin, unit).contains(at))
        });
    if !inside {
        return;
    }
    if ctx.input(|input| {
        input
            .pointer
            .button_double_clicked(egui::PointerButton::Primary)
    }) {
        app.actions.push(Action::MilkdropFullscreen);
    }
    if rect.width().min(rect.height()) < LEAST_PANE {
        return;
    }
    let button = Rect::from_min_size(
        egui::pos2(rect.right() - SIDE - 4.0, rect.top() + 4.0),
        egui::Vec2::splat(SIDE),
    );
    keep_pane_button(&ctx, button);
    let response = ui
        .interact(button, Id::new("wmp-pane-full-screen"), Sense::click())
        .on_hover_text(gettext(app.locale, "Full screen with lyrics").as_ref());
    ui.painter()
        .with_clip_rect(rect)
        .rect_filled(button, 4.0, Color32::from_black_alpha(150));
    crate::theme::paint_icon(ui, crate::theme::Icon::Expand, button, 14.0, Color32::WHITE);
    if response.clicked() {
        app.actions.push(Action::MilkdropFullscreen);
    }
}

fn key_code(key: egui::Key) -> Option<u32> {
    use egui::Key;
    let name = key.name();
    if name.len() == 1 {
        return name
            .chars()
            .next()
            .map(|c| u32::from(c.to_ascii_uppercase()));
    }
    Some(match key {
        Key::Enter => 13,
        Key::Escape => 27,
        Key::Backspace => 8,
        Key::Tab => 9,
        Key::Space => 32,
        Key::ArrowLeft => 37,
        Key::ArrowUp => 38,
        Key::ArrowRight => 39,
        Key::ArrowDown => 40,
        Key::Delete => 46,
        _ => return None,
    })
}

/// Hands this frame's pointer and keys to the skin, in order.
fn read_input(app: &mut App, ctx: &egui::Context, key: u32, origin: Pos2, unit: f32) {
    let events = ctx.input(|input| input.events.clone());
    let ours = take_pane_button(ctx);
    let modifiers = ctx.input(|input| input.modifiers);
    let modifiers = Modifiers {
        shift: modifiers.shift,
        ctrl: modifiers.ctrl || modifiers.mac_cmd,
        alt: modifiers.alt,
    };
    let mut push = |input: Input| {
        app.actions.push(Action::WmpInput {
            view: key,
            input,
            modifiers,
        });
    };
    let at = |pos: Pos2| {
        let place = (pos - origin) / unit;
        Input::Pointer {
            x: place.x,
            y: place.y,
        }
    };
    for event in events {
        match event {
            egui::Event::PointerMoved(pos) => push(at(pos)),
            egui::Event::PointerGone => push(Input::Left),
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                ..
            } => {
                let secondary = button == egui::PointerButton::Secondary;
                if pressed && on_our_button(ours, pos) {
                    continue;
                }
                if secondary || button == egui::PointerButton::Primary {
                    push(at(pos));
                    push(if pressed {
                        Input::Press { secondary }
                    } else {
                        Input::Release { secondary }
                    });
                }
            }
            egui::Event::MouseWheel { delta, unit, .. } => {
                let lines = match unit {
                    egui::MouseWheelUnit::Line => delta.y,
                    egui::MouseWheelUnit::Point => delta.y / 20.0,
                    egui::MouseWheelUnit::Page => delta.y * 3.0,
                };
                push(Input::Wheel(lines));
            }
            egui::Event::Key {
                key: pressed_key,
                pressed,
                repeat: false,
                ..
            } => {
                if let Some(code) = key_code(pressed_key) {
                    push(Input::Key { code, pressed });
                }
            }
            egui::Event::Text(text) => {
                for character in text.chars() {
                    push(Input::Character(character));
                }
            }
            _ => {}
        }
    }
}

fn world(area: Box2, origin: Pos2, unit: f32) -> Rect {
    Rect::from_min_size(
        origin + vec2(area.x as f32, area.y as f32) * unit,
        vec2(area.width as f32, area.height as f32) * unit,
    )
}

/// Draws one thing; returns whether it moves by itself.
fn draw(app: &mut App, ui: &mut Ui, paint: &Paint, origin: Pos2, unit: f32, now: Instant) -> bool {
    match paint {
        Paint::Picture {
            picture,
            frame,
            x,
            y,
            part,
            stretch,
            alpha,
            clip,
        } => {
            let Some(bitmap) = picture.frames.get(*frame).map(|frame| &frame.bitmap) else {
                return false;
            };
            if part.width <= 0 || part.height <= 0 || bitmap.width == 0 || bitmap.height == 0 {
                return false;
            }
            let ctx = ui.ctx().clone();
            let texture = app.winamp.wmp_texture(&ctx, picture, *frame);
            let (width, height) = (bitmap.width as f32, bitmap.height as f32);
            let uv = Rect::from_min_max(
                pos2(part.x as f32 / width, part.y as f32 / height),
                pos2(
                    (part.x + part.width) as f32 / width,
                    (part.y + part.height) as f32 / height,
                ),
            );
            let (draw_width, draw_height) = stretch.unwrap_or((part.width, part.height));
            let rect = world(
                Box2 {
                    x: *x,
                    y: *y,
                    width: draw_width,
                    height: draw_height,
                },
                origin,
                unit,
            );
            let tint = Color32::from_white_alpha(*alpha);
            for box2 in clip.iter() {
                let area = world(*box2, origin, unit).intersect(ui.clip_rect());
                if area.intersects(rect) {
                    ui.painter()
                        .with_clip_rect(area)
                        .image(texture, rect, uv, tint);
                }
            }
            false
        }
        Paint::Fill {
            area,
            color,
            alpha,
            clip,
        } => {
            let fill = Color32::from_rgba_unmultiplied(color[0], color[1], color[2], *alpha);
            let rect = world(*area, origin, unit);
            for box2 in clip.iter() {
                let area = world(*box2, origin, unit).intersect(ui.clip_rect());
                ui.painter()
                    .with_clip_rect(area)
                    .rect_filled(rect, 0.0, fill);
            }
            false
        }
        Paint::Text { text, clip } => {
            draw_text(ui, text, clip, origin, unit);
            false
        }
        Paint::Effects { area, alpha, clip } => {
            let rect = world(*area, origin, unit);
            #[cfg(feature = "milkdrop")]
            let waiting = {
                use crate::milkdrop::pane::{Picture, UV};
                let picture = if app.settings.vis == VisMode::Off {
                    Picture::None
                } else {
                    let ctx = ui.ctx().clone();
                    let pixels = rect.size() * ctx.pixels_per_point();
                    app.milkdrop_picture(&ctx, [pixels.x, pixels.y])
                };
                if let Picture::Frame(texture) = picture {
                    let tint = Color32::from_white_alpha(*alpha);
                    for box2 in clip.iter() {
                        let area = world(*box2, origin, unit).intersect(ui.clip_rect());
                        ui.painter()
                            .with_clip_rect(area)
                            .image(texture, rect, UV, tint);
                    }
                    pane_full_screen(app, ui, rect, clip, origin, unit);
                    return true;
                }
                picture == Picture::Waiting
            };
            #[cfg(not(feature = "milkdrop"))]
            let waiting = false;
            let (shapes, moving) = effect_shapes(app, rect, unit, now);
            // Bars have no alpha of their own: past halfway through a fade.
            if *alpha >= 128 {
                for box2 in clip.iter() {
                    let area = world(*box2, origin, unit).intersect(ui.clip_rect());
                    ui.painter().with_clip_rect(area).extend(shapes.clone());
                }
            }
            moving || waiting
        }
        Paint::Wave {
            area,
            color,
            alpha,
            clip,
        } => {
            let rect = world(*area, origin, unit);
            let (shapes, moving) = wave_shapes(app, rect, *color, *alpha, unit);
            for box2 in clip.iter() {
                let area = world(*box2, origin, unit).intersect(ui.clip_rect());
                ui.painter().with_clip_rect(area).extend(shapes.clone());
            }
            moving
        }
        Paint::Art {
            area,
            background,
            clip,
        } => {
            let rect = world(*area, origin, unit);
            let bounds = clip
                .iter()
                .fold(Rect::NOTHING, |all, box2| {
                    all.union(world(*box2, origin, unit))
                })
                .intersect(ui.clip_rect());
            if let Some([r, g, b]) = background {
                ui.painter().with_clip_rect(bounds).rect_filled(
                    rect,
                    0.0,
                    Color32::from_rgb(*r, *g, *b),
                );
            }
            let url = app.now_playing().and_then(|now| now.art_url);
            if url.is_some() {
                let side = rect.width().min(rect.height());
                let cover = Rect::from_center_size(rect.center(), vec2(side, side));
                let saved = ui.clip_rect();
                ui.set_clip_rect(bounds);
                crate::ui::widgets::paint_cover(
                    ui,
                    &app.palette,
                    url.as_deref(),
                    cover,
                    0.0,
                    crate::theme::Icon::Music,
                    None,
                );
                ui.set_clip_rect(saved);
            }
            false
        }
    }
}

fn draw_text(ui: &Ui, text: &TextPaint, clip: &[Box2], origin: Pos2, unit: f32) {
    let size = text.size_px * unit;
    let font = if text.mono {
        FontId::monospace(size)
    } else if text.bold {
        fastframe_fonts::Weight::Bold.font_id(size)
    } else {
        fastframe_fonts::Weight::Regular.font_id(size)
    };
    let rect = world(text.area, origin, unit);
    let [r, g, b] = text.color;
    let color = Color32::from_rgba_unmultiplied(r, g, b, text.alpha);
    for box2 in clip {
        let area = world(*box2, origin, unit).intersect(ui.clip_rect());
        let painter = ui.painter().with_clip_rect(area);
        let galley = if text.wrap {
            painter.layout(
                text.text.clone(),
                font.clone(),
                color,
                rect.width().max(1.0),
            )
        } else {
            crate::bidi::layout_line(&painter, text.text.clone(), font.clone(), color)
        };
        let anchor = match text.align {
            -1 => Align2::LEFT_TOP,
            0 => Align2::CENTER_TOP,
            _ => Align2::RIGHT_TOP,
        };
        let x = match text.align {
            -1 => rect.min.x,
            0 => rect.center().x,
            _ => rect.max.x,
        };
        let at = anchor.anchor_size(pos2(x, rect.min.y), galley.size());
        let size = galley.size();
        // Whole pixels keep small type sharp.
        let start = pos2((at.min.x - text.scroll * unit).round(), at.min.y.round());
        painter.galley(start, galley.clone(), color);
        if text.scroll != 0.0 {
            let gap = 25.0 * unit;
            let step = if text.scroll > 0.0 {
                size.x + gap
            } else {
                -(size.x + gap)
            };
            painter.galley(start + vec2(step, 0.0), galley, color);
        }
        let stroke = egui::Stroke::new((size.y / 14.0).max(1.0), color);
        if text.underline {
            let y = start.y + size.y - 1.0;
            painter.line_segment([pos2(start.x, y), pos2(start.x + size.x, y)], stroke);
        }
        if text.strikeout {
            let y = start.y + size.y * 0.55;
            painter.line_segment([pos2(start.x, y), pos2(start.x + size.x, y)], stroke);
        }
    }
}

/// A strip of thin upright lines as tall as the sound, from the same place as
/// the visualisations: after the equalizer and before the volume.
fn wave_shapes(
    app: &App,
    rect: Rect,
    [r, g, b]: [u8; 3],
    alpha: u8,
    unit: f32,
) -> (Vec<Shape>, bool) {
    let sounding = app
        .now_playing()
        .is_some_and(|now| (now.playing || now.loading) && now.local);
    let samples = if sounding {
        app.winamp.tap.window(vis::SCOPE_SAMPLES, vis::LAG)
    } else {
        Vec::new()
    };
    let line = Color32::from_rgba_unmultiplied(r, g, b, alpha);
    let glow = Color32::from_rgba_unmultiplied(r, g, b, alpha / 5);
    let step = 3.0 * unit;
    let columns = (rect.width() / step).floor().max(1.0) as usize;
    let chunk = (samples.len() / columns).max(1);
    let mut shapes = Vec::with_capacity(columns * 2);
    for column in 0..columns {
        let peak = samples.chunks(chunk).nth(column).map_or(0.0, |part| {
            part.iter().fold(0.0f32, |top, at| top.max(at.abs()))
        });
        // Tapered to the ends, and quiet parts kept quiet.
        let taper = (std::f32::consts::PI * (column as f32 + 0.5) / columns as f32).sin();
        let height = (peak.min(1.0).powf(1.6) * taper.sqrt() * rect.height()).max(1.5 * unit);
        let x = rect.min.x + step * (column as f32 + 0.5);
        let bar = Rect::from_center_size(pos2(x, rect.center().y), vec2(1.5 * unit, height));
        shapes.push(Shape::rect_filled(bar.expand(unit), 2.0 * unit, glow));
        shapes.push(Shape::rect_filled(bar, 0.75 * unit, line));
    }
    (shapes, sounding)
}

/// The skin's visualisation: the same bars or wave the Winamp display
/// shows, from the sound after the equalizer and before the volume. Returns
/// its shapes and whether it is still moving.
pub(super) fn effect_shapes(
    app: &mut App,
    rect: Rect,
    unit: f32,
    now: Instant,
) -> (Vec<Shape>, bool) {
    let now_playing = app.now_playing();
    let sounding = now_playing
        .as_ref()
        .is_some_and(|now| (now.playing || now.loading) && now.local);
    let palette = app.winamp.skin.vis_colors;
    let color = |index: usize| {
        let [r, g, b] = palette[index.min(palette.len() - 1)];
        Color32::from_rgb(r, g, b)
    };
    let mut shapes = Vec::new();
    let moving = match app.settings.vis {
        VisMode::Off => false,
        VisMode::Bars => {
            let samples = if sounding {
                app.winamp.tap.window(vis::FFT_SAMPLES, vis::LAG)
            } else {
                vec![0.0; vis::FFT_SAMPLES]
            };
            let bars = app.winamp.analyser.step(&samples, now);
            let slot = rect.width() / bars.len() as f32;
            let row = rect.height() / f32::from(vis::ROWS);
            let block = |left: f32, top: f32, fill: Color32| {
                Shape::rect_filled(
                    Rect::from_min_size(pos2(left + slot * 0.1, top), vec2(slot * 0.8, row * 0.85)),
                    0.0,
                    fill,
                )
            };
            for (index, bar) in bars.iter().enumerate() {
                let left = rect.min.x + slot * index as f32;
                for level in (vis::ROWS - bar.height)..vis::ROWS {
                    let top = rect.min.y + row * f32::from(level);
                    shapes.push(block(left, top, color(2 + usize::from(level))));
                }
                if let Some(peak) = bar.peak {
                    let top = rect.min.y + row * f32::from(vis::ROWS - peak);
                    shapes.push(block(left, top, color(23)));
                }
            }
            sounding || !app.winamp.analyser.settled()
        }
        VisMode::Scope => {
            let samples = if sounding {
                app.winamp.tap.window(vis::SCOPE_SAMPLES, vis::LAG)
            } else {
                vec![0.0; vis::SCOPE_SAMPLES]
            };
            let rows = vis::scope(&samples);
            let step = rect.width() / rows.len() as f32;
            let row = rect.height() / f32::from(vis::ROWS);
            let points: Vec<Pos2> = rows
                .iter()
                .enumerate()
                .map(|(x, &y)| {
                    pos2(
                        rect.min.x + step * (x as f32 + 0.5),
                        rect.min.y + row * (f32::from(y) + 0.5),
                    )
                })
                .collect();
            for pair in points.windows(2) {
                let shade = color(18 + vis::scope_shade(((pair[0].y - rect.min.y) / row) as u8));
                shapes.push(Shape::line_segment(
                    [pair[0], pair[1]],
                    egui::Stroke::new(unit.max(1.5), shade),
                ));
            }
            sounding
        }
    };
    (shapes, moving)
}

#[cfg(test)]
mod pane_button_tests {
    use super::*;

    /// A press on the button is ours; anywhere else, and with no button
    /// showing, the skin has it.
    #[cfg(feature = "milkdrop")]
    #[test]
    fn the_pane_button_is_kept_and_taken_without_freezing_the_context() {
        let ctx = egui::Context::default();
        let button = Rect::from_min_size(Pos2::ZERO, egui::Vec2::splat(20.0));
        assert_eq!(take_pane_button(&ctx), None);
        keep_pane_button(&ctx, button);
        assert_eq!(take_pane_button(&ctx), Some(button));
        assert_eq!(take_pane_button(&ctx), None);
    }

    #[test]
    fn a_press_on_the_pane_button_does_not_reach_the_skin() {
        let button = Rect::from_min_size(pos2(10.0, 10.0), vec2(22.0, 22.0));
        assert!(on_our_button(Some(button), pos2(20.0, 20.0)));
        assert!(!on_our_button(Some(button), pos2(40.0, 20.0)));
        assert!(!on_our_button(None, pos2(20.0, 20.0)));
    }
}
