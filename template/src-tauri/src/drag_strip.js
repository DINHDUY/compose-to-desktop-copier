(() => {
  const ID = "tauri-drag-strip";
  const RESERVE = __CHROME_RESERVE__;

  const startDrag = (event) => {
    if (event.button !== 0) return;
    if (event.clientY < 0 || event.clientY > 12) return;
    if (window.innerWidth - event.clientX <= RESERVE) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    const invoke = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
    if (invoke) invoke("plugin:window|start_dragging", {});
  };

  const mount = () => {
    const parent = document.body || document.documentElement;
    if (!parent) return;
    let bar = document.getElementById(ID);
    if (!bar) {
      bar = document.createElement("div");
      bar.id = ID;
      bar.setAttribute("data-tauri-drag-region", "true");
      bar.style.cssText =
        "position:fixed;top:0;left:0;right:" +
        RESERVE +
        "px;height:12px;z-index:2147483647;background:transparent;cursor:default;pointer-events:auto;";
      parent.appendChild(bar);
    } else if (parent.lastElementChild !== bar) {
      parent.appendChild(bar);
    }
  };

  const boot = () => {
    mount();
    window.addEventListener("mousedown", startDrag, true);
    const root = document.documentElement;
    if (!root || root.dataset.tauriDragObserver) return;
    root.dataset.tauriDragObserver = "1";
    new MutationObserver(() => mount()).observe(root, { childList: true, subtree: true });
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot);
  } else {
    boot();
  }
})();
