// Planner list motion — evaluated once by CentreColumn (planner_rows.rs).
//
// hobbesFlip: FLIP (First, Last, Invert, Play) for re-sorts, regrouping and
// drops. `snap()` records every row's top before the Rust side writes the new
// order; `play()` runs from a use_effect after the re-render and animates each
// row from where it was to where it is. Evals share the DOM-edit channel, so
// play() always sees the new layout.
//
// hobbesDrag: the custom mouse-tracking drag (not HTML5 DnD — its ghost image
// can't be styled in WKWebView and it would fight the chat's file drop). The
// Rust overlay calls move() per mousemove; nothing re-renders mid-drag.
window.hobbesFlip = window.hobbesFlip || {
  first: null,
  snap() {
    this.first = new Map();
    document.querySelectorAll('[data-row-id]').forEach(el =>
      this.first.set(el.dataset.rowId, el.getBoundingClientRect().top));
  },
  play(duration) {
    const els = document.querySelectorAll('[data-row-id]');
    // Clear any drag transforms first so the "last" positions are real.
    document.body.style.userSelect = '';
    els.forEach(el => {
      el.style.transform = '';
      el.style.transition = '';
      el.style.zIndex = '';
      el.style.boxShadow = '';
      el.style.background = '';
    });
    const first = this.first;
    this.first = null;
    if (!first || matchMedia('(prefers-reduced-motion: reduce)').matches) return;
    els.forEach(el => {
      const f = first.get(el.dataset.rowId);
      const l = el.getBoundingClientRect().top;
      if (f === undefined) {
        el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 160 });
      } else if (Math.abs(f - l) > 1) {
        el.animate(
          [{ transform: `translateY(${f - l}px)` }, { transform: 'none' }],
          { duration: duration || 220, easing: 'cubic-bezier(.2,.7,.2,1)' });
      }
    });
  },
};

window.hobbesDrag = window.hobbesDrag || {
  // ids: the grabbed group's open rows in order; the row at `from` follows the
  // pointer by `dy`, and the rows it has passed step aside by one row height.
  move(ids, from, over, dy) {
    if (document.body.style.userSelect !== 'none') {
      document.body.style.userSelect = 'none';
      window.getSelection()?.removeAllRanges();
    }
    const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
    ids.forEach((id, i) => {
      const el = document.querySelector(`[data-row-id="${CSS.escape(id)}"]`);
      if (!el) return;
      if (i === from) {
        el.style.transition = 'none';
        el.style.transform = `translateY(${dy}px)`;
        el.style.zIndex = '30';
        el.style.boxShadow = '0 6px 18px rgba(0,0,0,.35)';
        el.style.background = 'var(--color-bg-section)';
        return;
      }
      const h = el.offsetHeight;
      let shift = 0;
      if (from < over && i > from && i <= over) shift = -h;
      if (from > over && i >= over && i < from) shift = h;
      el.style.transition = reduce ? 'none' : 'transform 120ms cubic-bezier(.2,.7,.2,1)';
      el.style.transform = shift ? `translateY(${shift}px)` : '';
    });
  },
};
