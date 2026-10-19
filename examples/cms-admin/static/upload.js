(() => {
  const form = document.getElementById("upload-form");
  if (!form) return;

  const input = document.getElementById("files");
  const zone = document.getElementById("drop-zone");
  const queue = document.getElementById("upload-queue");
  const button = document.getElementById("upload-button");
  const progress = document.getElementById("upload-progress");
  const progressName = document.getElementById("progress-name");
  const progressPercent = document.getElementById("progress-percent");
  const progressBar = document.getElementById("progress-bar");
  const results = document.getElementById("upload-results");
  let files = [];

  const renderQueue = () => {
    queue.replaceChildren();
    for (const file of files) {
      const row = document.createElement("div");
      row.className = "queue-file";
      const name = document.createElement("span");
      name.textContent = file.name;
      const size = document.createElement("small");
      size.textContent = `${(file.size / 1024 / 1024).toFixed(2)} MiB`;
      row.append(name, size);
      queue.append(row);
    }
    button.disabled = files.length === 0;
    button.textContent = files.length ? `Upload ${files.length} file${files.length === 1 ? "" : "s"}` : "Upload selected files";
  };

  input.addEventListener("change", () => {
    files = [...input.files];
    renderQueue();
  });
  for (const eventName of ["dragenter", "dragover"]) {
    zone.addEventListener(eventName, (event) => {
      event.preventDefault();
      zone.classList.add("dragover");
    });
  }
  for (const eventName of ["dragleave", "drop"]) {
    zone.addEventListener(eventName, (event) => {
      event.preventDefault();
      zone.classList.remove("dragover");
    });
  }
  zone.addEventListener("drop", (event) => {
    files = [...event.dataTransfer.files];
    renderQueue();
  });

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!files.length) return;
    const body = new FormData();
    body.append("kind", document.getElementById("upload-kind").value);
    for (const file of files) body.append("files", file, file.name);

    button.disabled = true;
    results.replaceChildren();
    progress.hidden = false;
    progressName.textContent = `Uploading ${files.length} file${files.length === 1 ? "" : "s"}…`;
    progressPercent.textContent = "0%";
    progressBar.style.width = "0%";

    const xhr = new XMLHttpRequest();
    xhr.open("POST", form.action);
    xhr.setRequestHeader("HX-Request", "true");
    xhr.upload.addEventListener("progress", (event) => {
      if (!event.lengthComputable) return;
      const percent = Math.round(event.loaded * 100 / event.total);
      progressPercent.textContent = `${percent}%`;
      progressBar.style.width = `${percent}%`;
    });
    xhr.addEventListener("load", () => {
      progress.hidden = true;
      if (xhr.status >= 200 && xhr.status < 300) {
        results.innerHTML = xhr.responseText;
        files = [];
        input.value = "";
        renderQueue();
      } else {
        results.textContent = `Upload request failed (${xhr.status}).`;
        button.disabled = false;
      }
    });
    xhr.addEventListener("error", () => {
      progress.hidden = true;
      results.textContent = "The upload could not reach the dashboard.";
      button.disabled = false;
    });
    xhr.send(body);
  });
})();
