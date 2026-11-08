const search = document.querySelector("#search");
const results = document.querySelector("#search-results");
const status = document.querySelector("#search-status");
const home = new URL(document.querySelector(".brand").href);
const mobile = window.matchMedia("(max-width: 47rem)");
const navigation = document.querySelector("#navigation");

function updateNavigation() {
  navigation.open = !mobile.matches;
}

function closeSearch() {
  results.hidden = true;
  status.textContent = "";
}

function updateSearch() {
  const words = search.value.toLowerCase().trim().split(/\s+/).filter(Boolean);
  if (!words.length) {
    closeSearch();
    return;
  }

  const matches = [];
  for (const [title, heading, path, body] of window.cpsSearch) {
    const labels = `${title} ${heading}`.toLowerCase();
    const content = `${labels} ${body.toLowerCase()}`;
    if (words.every((word) => content.includes(word))) {
      const score = words.filter((word) => labels.includes(word)).length;
      matches.push({ title, heading, path, score });
    }
  }

  matches.sort((first, second) => second.score - first.score);
  results.replaceChildren();
  const shown = matches.slice(0, 8);
  if (shown.length) {
    const list = document.createElement("ul");
    for (const match of shown) {
      const item = document.createElement("li");
      const link = document.createElement("a");
      link.href = new URL(match.path, home);
      link.textContent = match.heading || match.title;
      if (match.heading) {
        const context = document.createElement("span");
        context.textContent = match.title;
        link.append(context);
      }
      item.append(link);
      list.append(item);
    }
    results.append(list);
    status.textContent = `${shown.length} search results shown`;
  } else {
    const message = document.createElement("p");
    message.textContent = "No matching sections.";
    results.append(message);
    status.textContent = message.textContent;
  }
  results.hidden = false;
}

updateNavigation();
mobile.addEventListener("change", updateNavigation);

if (window.cpsSearch) {
  search.closest(".search").hidden = false;
  search.addEventListener("input", updateSearch);
  search.addEventListener("focus", updateSearch);
  search.addEventListener("keydown", (event) => {
    const firstResult = results.querySelector("a");
    if (event.key === "Enter" && !results.hidden && firstResult) {
      event.preventDefault();
      firstResult.click();
    }
  });
  results.addEventListener("click", closeSearch);
  document.addEventListener("click", (event) => {
    if (!event.target.closest(".search")) closeSearch();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !results.hidden) {
      search.focus();
      closeSearch();
    }
  });
}
