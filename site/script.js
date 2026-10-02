// copy-to-clipboard for the install command
document.querySelectorAll('.copy-btn').forEach((btn) => {
  btn.addEventListener('click', async () => {
    const target = document.querySelector(btn.dataset.clip);
    if (!target) return;
    const text = target.textContent.trim();
    try {
      await navigator.clipboard.writeText(text);
      btn.classList.add('copied');
      const original = btn.innerHTML;
      btn.innerHTML = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="20 6 9 17 4 12"/></svg>';
      setTimeout(() => {
        btn.classList.remove('copied');
        btn.innerHTML = original;
      }, 1600);
    } catch (err) {
      console.error('clipboard write failed', err);
    }
  });
});

// fade-in on scroll for sections (hero stays visible)
const observer = new IntersectionObserver(
  (entries) => {
    entries.forEach((e) => {
      if (e.isIntersecting) {
        e.target.style.opacity = '1';
        e.target.style.transform = 'translateY(0)';
      }
    });
  },
  { threshold: 0.08, rootMargin: '0px 0px -50px 0px' }
);
document.querySelectorAll('section').forEach((s) => {
  s.style.opacity = '0';
  s.style.transform = 'translateY(20px)';
  s.style.transition = 'opacity 0.6s ease-out, transform 0.6s ease-out';
  observer.observe(s);
});
const hero = document.querySelector('.hero');
if (hero) {
  hero.style.opacity = '1';
  hero.style.transform = 'translateY(0)';
}

// hero terminal: a scripted session that loops. Each frame is {title, html}.
const frames = [
  {
    title: '~/code/app  powerqueue run',
    html:
      '<span class="k">$</span> powerqueue run\n' +
      '<span class="b">powerqueue</span> repo ~/code/app | linear ENG | max 2 concurrent | tmux session powerqueue\n' +
      '<span class="d">INFO</span> loaded ~/.config/powerqueue/PRIORITY.md (0 warnings)\n' +
      '<span class="d">INFO</span> linear sync: <span class="g">+3 new</span> · 0 updated · 0 closed\n' +
      '<span class="d">INFO</span> <span class="y">ENG-412</span> critical → <span class="p">fable</span> (incident label, period 18% elapsed)\n' +
      '<span class="d">INFO</span> created worktree …/worktrees/app/eng-412 on <span class="c">pq/eng-412</span>\n' +
      '<span class="d">INFO</span> launched <span class="p">fable</span> session (attempt 1) in window <span class="c">@3</span>\n' +
      '<span class="d">INFO</span> <span class="y">ENG-398</span> normal → <span class="c">sonnet</span> (default; fable reserved for critical)\n' +
      '<span class="d">INFO</span> launched <span class="c">sonnet</span> session (attempt 1) in window <span class="c">@4</span>\n' +
      '<span class="d">WARN</span> <span class="y">ENG-398</span> Claude exited with status 1; retrying in 30s (attempt 1 of 3)\n' +
      '<span class="d">INFO</span> launched <span class="c">sonnet</span> session (attempt 2, <span class="g">resumed</span>) in window <span class="c">@5</span>\n' +
      '<span class="d">INFO</span> <span class="y">ENG-412</span> task completed: hotfix shipped, tests green\n' +
      '<span class="d">INFO</span> pushed <span class="c">pq/eng-412</span> · removed worktree · Linear → In Review\n' +
      '<span class="cursor">▌</span>',
  },
  {
    title: '~  powerqueue dashboard',
    html:
      '<span class="b"> powerqueue</span> <span class="g">●</span> daemon running  slots 2/2  period <span class="y">41%</span> (elapsed 46%)  window 12%\n' +
      '<span class="d">┌ tasks (4 open) ───────────────────────────────────────────────┐</span>\n' +
      '<span class="d">│</span> <span class="d">KEY      STATE     CRIT      MODEL   TOKENS  CPU   RSS     AGE</span>  <span class="d">│</span>\n' +
      '<span class="d">│</span><span class="sel">▶ ENG-412  running   <span class="r">critical</span>  <span class="p">fable</span>   2.1M    38%   612 MB  14m</span> <span class="d">│</span>\n' +
      '<span class="d">│</span>  ENG-398  <span class="g">running</span>   normal    <span class="c">sonnet</span>  640k    12%   340 MB  6m   <span class="d">│</span>\n' +
      '<span class="d">│</span>  ENG-401  <span class="m">throttled</span> high      -       -       -     -       2m   <span class="d">│</span>\n' +
      '<span class="d">│</span>  ENG-377  queued    low       -       -       -     -       1h   <span class="d">│</span>\n' +
      '<span class="d">└───────────────────────────────────────────────────────────────┘</span>\n' +
      '<span class="d">┌ budget ──────────────────────┐</span>  <span class="d">┌ ENG-412 ───────────────────┐</span>\n' +
      '<span class="d">│</span> <span class="p">fable </span> <span class="p">████████</span>░░░░░░ 31% / 25% <span class="d">│</span>  <span class="d">│</span> attempt 1 · branch pq/eng-412 <span class="d">│</span>\n' +
      '<span class="d">│</span> <span class="b">opus  </span> <span class="b">████</span>░░░░░░░░░░ 14% / 35% <span class="d">│</span>  <span class="d">│</span> 14:02 session.started        <span class="d">│</span>\n' +
      '<span class="d">│</span> <span class="c">sonnet</span> <span class="c">██████████</span>░░░░ 58% / 35% <span class="d">│</span>  <span class="d">│</span> 14:11 usage 2.1M weighted    <span class="d">│</span>\n' +
      '<span class="d">│</span> haiku  ░░░░░░░░░░░░░░  0% /  5% <span class="d">│</span>  <span class="d">│</span> 14:16 commit: fix retry loop <span class="d">│</span>\n' +
      '<span class="d">└──────────────────────────────┘</span>  <span class="d">└────────────────────────────┘</span>\n' +
      '<span class="d"> ↑↓ select  a attach  p pause  r resume  c cancel  R retry  n new  ? help  q quit</span>',
  },
  {
    title: '~  powerqueue doctor',
    html:
      '<span class="k">$</span> powerqueue doctor\n' +
      '<span class="b">environment</span>\n' +
      '  <span class="g">✓</span> git          git version 2.50.1\n' +
      '  <span class="g">✓</span> tmux         tmux 3.7\n' +
      '  <span class="g">✓</span> claude       2.1.287 · logged in (claude.ai)\n' +
      '<span class="b">algorithm</span>\n' +
      '  <span class="g">✓</span> cost estimator   23 completed tasks · predictions within 18%\n' +
      '  <span class="y">!</span> fable reservation  12% of Fable spent, 71% of the period elapsed\n' +
      '    <span class="d">fix: lower budget.models.fable.relax_after_fraction (0.5 → 0.35)</span>\n' +
      '  <span class="g">✓</span> crash rate       2 crashes over 31 launches (6%)\n' +
      '  <span class="g">✓</span> window pressure  12% of the 5h window budget spent\n' +
      '\n<span class="g">21 ok</span>, <span class="y">1 warning</span>, 0 failures\n' +
      '<span class="cursor">▌</span>',
  },
];

const screen = document.getElementById('term-screen');
const title = document.getElementById('term-title');
if (screen && title) {
  let i = 0;
  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const show = () => {
    title.textContent = frames[i].title;
    screen.innerHTML = frames[i].html;
    screen.classList.remove('flash');
    void screen.offsetWidth; // restart the fade animation
    screen.classList.add('flash');
    i = (i + 1) % frames.length;
  };
  show();
  if (!reduced) setInterval(show, 6500);
}
