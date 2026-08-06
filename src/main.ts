const HOME_TITLE = "Página_principal";

function normalizeTitle(input: string): string {
  const trimmed = input.trim();
  if (!trimmed) return "";
  // MediaWiki style: spaces become underscores.
  return trimmed.replace(/\s+/g, "_");
}

function navigate(iframe: HTMLIFrameElement, title: string): void {
  const normalized = normalizeTitle(title);
  if (!normalized) return;
  iframe.src = "wiki://localhost/" + encodeURIComponent(normalized);
}

function bindEvents(): void {
  const iframe = document.querySelector<HTMLIFrameElement>("#wiki-frame");
  const form = document.querySelector<HTMLFormElement>("#search-form");
  const input = document.querySelector<HTMLInputElement>("#search-input");
  const brand = document.querySelector<HTMLElement>("#brand");

  if (!iframe) return;

  form?.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!input) return;
    navigate(iframe, input.value);
  });

  brand?.addEventListener("click", () => {
    navigate(iframe, HOME_TITLE);
  });
}

window.addEventListener("DOMContentLoaded", bindEvents);
