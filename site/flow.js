// A small, deterministic SVG demo. No timers run while it is offscreen or paused.
(() => {
  const svg = document.querySelector('.pipeline');
  if (!svg) return;

  const figure = svg.closest('figure');
  const toggle = figure.querySelector('.pipeline-toggle');
  const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)');
  const narrow = window.matchMedia('(max-width: 759px)');
  const slots = [...svg.querySelectorAll('[data-slot]')];
  const tickets = [...svg.querySelectorAll('[data-ticket]')];
  const packets = [...svg.querySelectorAll('[data-packet]')];
  const tasks = [
    { key: 'ENG-412', model: 'fable', start: 0, duration: 5.5 },
    { key: 'ENG-398', model: 'sonnet', start: 2.4, duration: 7 },
    { key: 'ENG-401', model: 'opus', start: 4.8, duration: 6 },
  ];
  const routes = tasks.map(() => ({ incoming: [], outgoing: [] }));
  const cycle = 18;
  // Reduced motion opens on a useful still: all three slots occupied.
  let elapsed = reducedMotion.matches ? 7.2 : 0;
  let lastFrame = null;
  let frame = null;
  let visible = false;
  let paused = reducedMotion.matches;

  const position = (selector, x, y) => {
    svg.querySelector(selector).setAttribute('transform', `translate(${x} ${y})`);
  };

  function layout() {
    const mobile = narrow.matches;
    svg.setAttribute('viewBox', mobile ? '0 0 400 1000' : '0 0 1120 460');
    position('.pipeline-queue', mobile ? 100 : 24, mobile ? 16 : 112);
    position('.pipeline-vps', mobile ? 24 : 294, mobile ? 286 : 40);
    position('.pipeline-policy', mobile ? 40 : 318, mobile ? 370 : 128);
    position('.pipeline-monitor', mobile ? 40 : 318, mobile ? 775 : 395);
    position('.pipeline-review', mobile ? 103 : 902, mobile ? 858 : 160);
    const shell = svg.querySelector('.pipeline-vps-shell');
    shell.setAttribute('width', mobile ? 352 : 540);
    shell.setAttribute('height', mobile ? 516 : 382);
    svg.querySelector('.pipeline-capacity').setAttribute('x', mobile ? 328 : 516);
    svg.querySelector('.pipeline-policy-shell').setAttribute('width', mobile ? 320 : 492);
    svg.querySelector('.pipeline-monitor text').textContent = mobile
      ? 'watch · resume · free slots'
      : 'watch usage · resume crashes · free slots';
    svg.querySelector('.pipeline-inlet').setAttribute('d', mobile
      ? 'M200 228 V262 H12 V401 H40' : 'M224 218 H264 V159 H318');
    svg.querySelector('.pipeline-outlet').setAttribute('d', mobile
      ? 'M276 673 H390 V916 H297' : 'M810 295 H862 V218 H902');

    slots.forEach((slot, i) => {
      const x = mobile ? [40, 208, 124][i] : 318 + i * 170;
      const y = mobile ? [442, 442, 602][i] : 224;
      const center = x + 76;
      slot.setAttribute('transform', `translate(${x} ${y})`);
      svg.querySelector(`[data-route="${i}"]`).setAttribute('d', mobile
        ? `M200 432 V${i === 2 ? 592 : 437} H${center} V${y}`
        : `M564 190 V207 H${center} V224`);
      routes[i].incoming = mobile
        ? [[200, 228], [200, 262], [12, 262], [12, 401], [40, 401], [200, 401], [200, i === 2 ? 592 : 437], [center, i === 2 ? 592 : 437], [center, y]]
        : [[224, 218], [264, 218], [264, 159], [318, 159], [564, 159], [564, 207], [center, 207], [center, y]];
      routes[i].outgoing = mobile
        ? [[x + 152, y + 71], [i === 0 ? 200 : 390, y + 71], [i === 0 ? 200 : 390, i === 0 ? 594 : 820], [390, i === 0 ? 594 : 820], [390, 916], [297, 916]]
        : [[center, y + 142], [center, 380], [862, 380], [862, 218], [902, 218]];
    });
    render();
  }

  // Interpolate along orthogonal paths at a constant speed.
  function move(packet, points, progress) {
    const lengths = points.slice(1).map((point, i) => Math.hypot(point[0] - points[i][0], point[1] - points[i][1]));
    let remaining = lengths.reduce((sum, length) => sum + length, 0) * progress;
    for (let i = 0; i < lengths.length; i++) {
      if (remaining <= lengths[i] || i === lengths.length - 1) {
        const fraction = lengths[i] ? remaining / lengths[i] : 0;
        const x = points[i][0] + (points[i + 1][0] - points[i][0]) * fraction;
        const y = points[i][1] + (points[i + 1][1] - points[i][1]) * fraction;
        packet.setAttribute('transform', `translate(${x} ${y})`);
        return;
      }
      remaining -= lengths[i];
    }
  }

  function render() {
    const time = elapsed % cycle;
    let completed = 0;
    tasks.forEach((task, i) => {
      const age = time - task.start;
      const running = age >= 2 && age < 2 + task.duration;
      const finishing = age >= 2 + task.duration && age < 3.8 + task.duration;
      const done = age >= 3.8 + task.duration;
      const slot = slots[i];
      slot.querySelector('.pipeline-slot-state').textContent = running ? task.key : finishing ? 'Complete ✓' : 'Available';
      slot.querySelector('.pipeline-slot-model').textContent = running ? task.model : finishing ? 'freeing slot…' : 'awaiting task';
      slot.querySelector('.pipeline-slot-fill').setAttribute('opacity', running ? '0.10' : finishing ? '0.16' : '0');
      slot.querySelector('.pipeline-slot-led').setAttribute('opacity', running || finishing ? '1' : '0.2');
      slot.querySelector('.pipeline-progress').setAttribute('width', running ? 124 * (age - 2) / task.duration : finishing ? 124 : 0);
      tickets[i].setAttribute('opacity', age < 0 ? '1' : age < 2 ? '0.65' : '0.25');
      const entering = age >= 0 && age < 2;
      packets[i].setAttribute('opacity', entering || finishing ? '1' : '0');
      if (entering) move(packets[i], routes[i].incoming, age / 2);
      if (finishing) move(packets[i], routes[i].outgoing, (age - 2 - task.duration) / 1.8);
      if (done) completed++;
    });
    svg.querySelector('.pipeline-review-state').textContent = completed
      ? `${completed} ${completed === 1 ? 'task' : 'tasks'} ready ✓` : 'Waiting for work';
  }

  function tick(timestamp) {
    if (lastFrame !== null) elapsed += (timestamp - lastFrame) / 1000;
    lastFrame = timestamp;
    render();
    frame = requestAnimationFrame(tick);
  }

  function updatePlayback() {
    if (frame !== null) cancelAnimationFrame(frame);
    frame = null;
    lastFrame = null;
    toggle.textContent = paused ? 'Play animation' : 'Pause animation';
    toggle.setAttribute('aria-pressed', String(paused));
    if (visible && !paused && !document.hidden) frame = requestAnimationFrame(tick);
  }

  toggle.hidden = false;
  toggle.addEventListener('click', () => {
    paused = !paused;
    updatePlayback();
  });
  reducedMotion.addEventListener('change', () => {
    paused = reducedMotion.matches;
    render();
    updatePlayback();
  });
  narrow.addEventListener('change', layout);
  document.addEventListener('visibilitychange', updatePlayback);
  new IntersectionObserver(([entry]) => {
    visible = entry.isIntersecting;
    updatePlayback();
  }, { threshold: 0.1 }).observe(figure);
  layout();
  updatePlayback();
})();
