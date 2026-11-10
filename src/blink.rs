//! Fase del parpadeo del cursor. Es solo un reloj: no sabe nada de ventanas ni de renderizado, así
//! que se prueba con instantes sintéticos.

use std::time::{Duration, Instant};

pub struct Blink {
    visible: bool,
    interval: Duration,
    deadline: Instant,
}

impl Blink {
    pub fn new(interval: Duration, now: Instant) -> Self {
        Self {
            visible: true,
            interval,
            deadline: now + interval,
        }
    }

    /// ¿Toca dibujar el cursor en esta fase?
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Cuándo hay que volver a alternar la fase
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn set_interval(&mut self, interval: Duration, now: Instant) {
        if interval != self.interval {
            self.interval = interval;
            self.deadline = now + interval;
        }
    }

    /// Alterna la fase si ya venció. Devuelve `true` si cambió (hay que redibujar).
    pub fn tick(&mut self, now: Instant) -> bool {
        if now < self.deadline {
            return false;
        }
        self.visible = !self.visible;
        self.deadline = now + self.interval;
        true
    }

    /// Vuelve a mostrar el cursor y reinicia el ciclo: mientras se escribe o llega salida el
    /// cursor se queda fijo. Devuelve `true` si estaba oculto (hay que redibujar).
    pub fn reset(&mut self, now: Instant) -> bool {
        let was_hidden = !self.visible;
        self.visible = true;
        self.deadline = now + self.interval;
        was_hidden
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_millis(500);

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn starts_visible_and_waits_a_full_interval() {
        let t0 = Instant::now();
        let blink = Blink::new(INTERVAL, t0);
        assert!(blink.visible());
        assert_eq!(blink.deadline(), at(t0, 500));
    }

    #[test]
    fn ticks_before_the_deadline_do_nothing() {
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        assert!(!blink.tick(at(t0, 499)));
        assert!(blink.visible());
        assert_eq!(blink.deadline(), at(t0, 500));
    }

    #[test]
    fn alternates_at_each_deadline() {
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        assert!(blink.tick(at(t0, 500)));
        assert!(!blink.visible());
        assert_eq!(blink.deadline(), at(t0, 1000));
        assert!(blink.tick(at(t0, 1000)));
        assert!(blink.visible());
    }

    #[test]
    fn a_late_tick_does_not_try_to_catch_up() {
        // Si el bucle se atrasó varios intervalos, solo se alterna una vez y el siguiente plazo
        // cuenta desde ahora: no hay ráfagas de parpadeos acumulados
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        assert!(blink.tick(at(t0, 5000)));
        assert!(!blink.visible());
        assert_eq!(blink.deadline(), at(t0, 5500));
        assert!(!blink.tick(at(t0, 5001)));
    }

    #[test]
    fn reset_shows_the_cursor_and_restarts_the_cycle() {
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        blink.tick(at(t0, 500));
        assert!(!blink.visible());

        assert!(blink.reset(at(t0, 700)), "estaba oculto: hay que redibujar");
        assert!(blink.visible());
        assert_eq!(blink.deadline(), at(t0, 1200));
        // Con el cursor ya visible no hace falta redibujar
        assert!(!blink.reset(at(t0, 800)));
        assert_eq!(blink.deadline(), at(t0, 1300));
    }

    #[test]
    fn typing_keeps_the_cursor_solid() {
        // Cada pulsación reinicia el plazo, así que mientras se escribe nunca llega a alternar
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        for step in 1..=10 {
            let now = at(t0, step * 300);
            blink.reset(now);
            assert!(!blink.tick(now + Duration::from_millis(100)));
            assert!(blink.visible());
        }
    }

    #[test]
    fn changing_the_interval_restarts_the_deadline_only_when_it_differs() {
        let t0 = Instant::now();
        let mut blink = Blink::new(INTERVAL, t0);
        blink.set_interval(INTERVAL, at(t0, 100));
        assert_eq!(blink.deadline(), at(t0, 500), "mismo intervalo: no se toca");
        blink.set_interval(Duration::from_millis(200), at(t0, 100));
        assert_eq!(blink.deadline(), at(t0, 300));
    }
}
