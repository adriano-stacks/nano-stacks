(() => {
  const slides = [...document.querySelectorAll('.slide')];
  const counter = document.querySelector('#counter');
  const progress = document.querySelector('#progress');
  let index = Number.parseInt(location.hash.slice(1), 10) - 1;
  if (!Number.isInteger(index) || index < 0 || index >= slides.length) index = 0;

  function show(next, updateHash = true) {
    index = Math.max(0, Math.min(slides.length - 1, next));
    slides.forEach((slide, at) => slide.classList.toggle('active', at === index));
    counter.textContent = `${index + 1} / ${slides.length}`;
    progress.style.width = `${((index + 1) / slides.length) * 100}%`;
    document.title = `${slides[index].dataset.title} — nano-stacks`;
    if (updateHash) history.replaceState(null, '', `#${index + 1}`);
  }

  document.querySelector('#prev').addEventListener('click', () => show(index - 1));
  document.querySelector('#next').addEventListener('click', () => show(index + 1));
  addEventListener('keydown', (event) => {
    if (['ArrowRight', 'ArrowDown', 'PageDown', ' ', 'Enter'].includes(event.key)) {
      event.preventDefault();
      show(index + 1);
    } else if (['ArrowLeft', 'ArrowUp', 'PageUp', 'Backspace'].includes(event.key)) {
      event.preventDefault();
      show(index - 1);
    } else if (event.key === 'Home') {
      show(0);
    } else if (event.key === 'End') {
      show(slides.length - 1);
    } else if (event.key.toLowerCase() === 'f') {
      document.documentElement.requestFullscreen?.();
    }
  });
  addEventListener('hashchange', () => {
    const requested = Number.parseInt(location.hash.slice(1), 10) - 1;
    if (Number.isInteger(requested)) show(requested, false);
  });
  show(index, false);

  if (new URLSearchParams(location.search).has('selftest')) {
    const start = index;
    const failures = [];
    show(0);
    document.querySelector('#next').click();
    if (index !== 1 || counter.textContent !== `2 / ${slides.length}`) {
      failures.push('next button');
    }
    dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowLeft' }));
    if (index !== 0) failures.push('previous key');
    dispatchEvent(new KeyboardEvent('keydown', { key: 'End' }));
    if (index !== slides.length - 1) failures.push('end key');
    location.hash = '#3';
    dispatchEvent(new HashChangeEvent('hashchange'));
    if (index !== 2) failures.push('hash navigation');

    const overflow = [];
    slides.forEach((slide, at) => {
      show(at, false);
      if (slide.scrollWidth > slide.clientWidth + 1 || slide.scrollHeight > slide.clientHeight + 1) {
        overflow.push(`${at + 1}: ${slide.dataset.title}`);
      }
    });
    show(start, false);

    const report = document.createElement('pre');
    report.id = 'deck-selftest';
    report.hidden = true;
    report.textContent = JSON.stringify({
      slides: slides.length,
      viewport: [innerWidth, innerHeight],
      navigation_failures: failures,
      overflow,
    });
    document.body.append(report);
    document.documentElement.dataset.selftest = failures.length || overflow.length ? 'failed' : 'passed';
  }
})();
