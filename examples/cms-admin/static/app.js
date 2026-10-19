(() => {
  const updateSelection = () => {
    const selected = [...document.querySelectorAll(".asset-checkbox:checked")];
    const count = document.getElementById("selected-count");
    const submit = document.getElementById("bulk-submit");
    if (count) count.textContent = `${selected.length} selected`;
    if (submit) submit.disabled = selected.length === 0;
  };

  document.addEventListener("change", (event) => {
    if (event.target.matches(".asset-checkbox")) updateSelection();
  });

  document.addEventListener("htmx:afterSwap", updateSelection);
  document.addEventListener("DOMContentLoaded", updateSelection);

  window.prepareBulk = (form) => {
    const ids = [...document.querySelectorAll(".asset-checkbox:checked")].map((box) => box.value);
    if (ids.length === 0) return false;
    if (["delete", "restore"].includes(form.elements.action.value)) {
      const verb = form.elements.action.value === "delete" ? "move to the trash" : "restore";
      if (!window.confirm(`${verb[0].toUpperCase()}${verb.slice(1)} ${ids.length} selected assets?`)) return false;
    }
    form.elements.ids.value = ids.join(",");
    return true;
  };

  document.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-copy]");
    if (!button) return;
    event.preventDefault();
    try {
      await navigator.clipboard.writeText(button.dataset.copy);
      const old = button.textContent;
      button.textContent = "Copied";
      window.setTimeout(() => { button.textContent = old; }, 1100);
    } catch {
      button.textContent = "Copy unavailable";
    }
  });
})();
