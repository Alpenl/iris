//! Image coordinates are physical pixels; viewport and pointer coordinates are
//! logical window units. mpv pan uses fractions of the displayed image size.

#[derive(Clone, Copy, Debug, Default)]
pub struct ImageView {
    pub zoom: f64,
    pub pan: [f64; 2],
}

impl ImageView {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn zoomed(&self) -> bool {
        self.zoom.abs() > 0.0001 || self.pan != [0., 0.]
    }

    pub fn displayed(&self, viewport: [f64; 2], media: [f64; 2]) -> [f64; 2] {
        let scale = fit(viewport, media) * self.zoom.exp2();
        [media[0] * scale, media[1] * scale]
    }

    /// Physical screen pixels per image pixel; 1.0 is actual size.
    pub fn pixel_scale(&self, viewport: [f64; 2], media: [f64; 2], backing_scale: f64) -> f64 {
        fit(viewport, media) * self.zoom.exp2() * backing_scale
    }

    /// Keeps the media point beneath the pointer fixed while changing scale.
    pub fn zoom_at(&mut self, delta: f64, pointer: [f64; 2], viewport: [f64; 2], media: [f64; 2]) {
        if viewport[0] <= 0. || viewport[1] <= 0. {
            return;
        }
        let old = self.displayed(viewport, media);
        let zoom = (self.zoom + delta).clamp(-4., 6.);
        let factor = (zoom - self.zoom).exp2();
        for axis in 0..2 {
            let anchor = pointer[axis] - viewport[axis] * 0.5;
            let old_offset = self.pan[axis] * old[axis];
            self.pan[axis] =
                (anchor - (anchor - old_offset) * factor) / (old[axis] * factor).max(1.);
        }
        self.zoom = zoom;
        self.clamp(viewport, media);
    }

    pub fn pan_by(&mut self, delta: [f64; 2], viewport: [f64; 2], media: [f64; 2]) {
        let displayed = self.displayed(viewport, media);
        for axis in 0..2 {
            self.pan[axis] += delta[axis] / displayed[axis].max(1.);
        }
        self.clamp(viewport, media);
    }

    pub fn toggle_actual_size(&mut self, viewport: [f64; 2], media: [f64; 2], backing_scale: f64) {
        if self.zoomed() {
            self.reset();
            return;
        }
        let fit = fit(viewport, media);
        self.zoom = (1. / (fit * backing_scale).max(0.00001))
            .log2()
            .clamp(-4., 6.);
        self.pan = [0., 0.];
    }

    fn clamp(&mut self, viewport: [f64; 2], media: [f64; 2]) {
        let displayed = self.displayed(viewport, media);
        for axis in 0..2 {
            let limit =
                ((displayed[axis] - viewport[axis]) / (2. * displayed[axis].max(1.))).max(0.);
            self.pan[axis] = self.pan[axis].clamp(-limit, limit);
        }
    }
}

/// The scale at which mpv shows the whole image in the viewport.
fn fit(viewport: [f64; 2], media: [f64; 2]) -> f64 {
    (viewport[0] / media[0].max(1.)).min(viewport[1] / media[1].max(1.))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_scale_reads_one_at_actual_size() {
        let mut view = ImageView::default();
        let (viewport, media) = ([800., 600.], [3200., 2400.]);
        assert!((view.pixel_scale(viewport, media, 2.) - 0.5).abs() < 1e-9);
        view.toggle_actual_size(viewport, media, 2.);
        assert!((view.pixel_scale(viewport, media, 2.) - 1.).abs() < 1e-9);
    }

    #[test]
    fn zoom_preserves_pointer_media_coordinate() {
        let mut view = ImageView::default();
        let viewport = [800., 600.];
        let media = [1600., 1200.];
        let pointer = [500., 360.];
        let before = view.displayed(viewport, media);
        let point = [
            (pointer[0] - 400.) / before[0],
            (pointer[1] - 300.) / before[1],
        ];
        view.zoom_at(1., pointer, viewport, media);
        let after = view.displayed(viewport, media);
        assert!(((pointer[0] - 400.) / after[0] - view.pan[0] - point[0]).abs() < 1e-9);
        assert!(((pointer[1] - 300.) / after[1] - view.pan[1] - point[1]).abs() < 1e-9);
    }

    #[test]
    fn pan_cannot_lose_image_and_reset_restores_fit() {
        let mut view = ImageView::default();
        view.pan_by([10000., -10000.], [800., 600.], [1600., 1200.]);
        assert_eq!(view.pan, [0., 0.]);
        view.zoom_at(1., [400., 300.], [800., 600.], [1600., 1200.]);
        view.pan_by([10000., -10000.], [800., 600.], [1600., 1200.]);
        assert_eq!(view.pan, [0.25, -0.25]);
        view.reset();
        assert_eq!(view.displayed([800., 600.], [1600., 1200.]), [800., 600.]);
    }
}
