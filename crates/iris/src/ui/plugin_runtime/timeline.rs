use super::super::{AppModel, views::model_listener};
use crate::theme::ThemeTokens;
use gpui::{
    AnyElement, App, Entity, InteractiveElement, IntoElement, MouseButton,
    StatefulInteractiveElement, Styled, div, px, relative,
};
use iris_api::v2::{
    Generation,
    enhance::{HeatRange, SkipRange},
};
use iris_core::player::Command;

fn can_seek_skip_range(
    rendered_generation: Generation,
    current_generation: Generation,
    has_timeline_ranges: bool,
) -> bool {
    current_generation == rendered_generation && has_timeline_ranges
}

impl AppModel {
    fn timeline_ranges(&self) -> Option<(&[HeatRange], &[SkipRange])> {
        self.plugins.timeline_ranges()
    }
}

pub(in crate::ui) fn plugin_timeline_ranges(
    model: &Entity<AppModel>,
    duration: f64,
    t: ThemeTokens,
    cx: &App,
) -> Vec<AnyElement> {
    if !duration.is_finite() || duration <= 0. {
        return Vec::new();
    }
    let m = model.read(cx);
    let Some((heat, skip)) = m.timeline_ranges() else {
        return Vec::new();
    };
    let ratio = |ms: u64| ((ms as f64 / 1000.) / duration).clamp(0., 1.) as f32;
    let mut marks = Vec::new();
    for range in heat {
        let (start, end) = (ratio(range.start_ms.0), ratio(range.end_ms.0));
        if end <= start {
            continue;
        }
        let mut color = t.accent;
        color.a = range.intensity as f32 / 100.;
        marks.push(
            div()
                .absolute()
                .top(px(3.))
                .left(relative(start))
                .w(relative(end - start))
                .h(px(2.))
                .bg(color)
                .into_any_element(),
        );
    }
    let generation = m.plugins.generation();
    for (index, range) in skip.iter().enumerate() {
        let (start, end) = (ratio(range.start_ms.0), ratio(range.end_ms.0));
        if end <= start {
            continue;
        }
        let target = (range.end_ms.0 as f64 / 1000.).min(duration);
        marks.push(
            div()
                .id(("plugin-skip", index))
                .absolute()
                .top(px(10.))
                .left(relative(start))
                .w(relative(end - start))
                .h(px(4.))
                .bg(t.text_muted)
                .cursor_pointer()
                .tooltip(crate::ui::kit::tip(format!("跳过 · {}", range.label), None))
                .on_mouse_down(
                    MouseButton::Left,
                    model_listener(model, move |this, _, _, cx| {
                        cx.stop_propagation();
                        if can_seek_skip_range(
                            generation,
                            this.plugins.generation(),
                            this.timeline_ranges().is_some(),
                        ) {
                            this.command(Command::SeekAbsolute(target), cx);
                        }
                    }),
                )
                .into_any_element(),
        );
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::can_seek_skip_range;
    use iris_api::v2::{Generation, Id};

    #[test]
    fn skip_range_remains_clickable_within_generation_and_rejects_stale_media() {
        let cur = Generation {
            media: Id(4),
            seek: Id(0),
            context: Id(2),
        };
        let next_media = Generation {
            media: Id(5),
            seek: Id(0),
            context: Id(2),
        };
        assert!(can_seek_skip_range(cur, cur, true));
        assert!(!can_seek_skip_range(cur, cur, false));
        assert!(!can_seek_skip_range(cur, next_media, true));
    }
}
