// The big play button over the video. Without JavaScript the video keeps its
// own controls, so the page works without this file.
const video = document.getElementById("tour-video");
const play = document.querySelector(".player .play");

if (video && play) {
  const start = () => {
    play.hidden = true;
    video.controls = true;
    video.play().catch(() => {});
  };
  video.controls = false;
  play.hidden = false;
  play.addEventListener("click", start);
  video.addEventListener("play", () => {
    play.hidden = true;
    video.controls = true;
  });

  // A link with data-play scrolls up to the video and plays it from the start.
  for (const link of document.querySelectorAll("[data-play]")) {
    link.addEventListener("click", (event) => {
      event.preventDefault();
      const smooth = !window.matchMedia("(prefers-reduced-motion: reduce)").matches;
      video.scrollIntoView({ behavior: smooth ? "smooth" : "auto", block: "center" });
      video.currentTime = 0;
      start();
    });
  }
}

// Copy buttons for commands. Without the clipboard API the button stays hidden.
for (const button of document.querySelectorAll("[data-copy]")) {
  const source = document.getElementById(button.dataset.copy);
  if (!source || !navigator.clipboard) continue;
  button.hidden = false;
  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(source.textContent.trim());
      button.textContent = "Copied";
    } catch {
      button.textContent = "Select and copy";
    }
    setTimeout(() => (button.textContent = "Copy"), 2000);
  });
}
