// Renders ```mermaid blocks. The library is loaded from a pinned cdnjs URL with a subresource
// integrity hash, and only on pages that contain a diagram.
(function () {
  var blocks = document.querySelectorAll("pre > code.language-mermaid");
  if (!blocks.length) return;
  var script = document.createElement("script");
  script.src = "https://cdnjs.cloudflare.com/ajax/libs/mermaid/10.9.1/mermaid.min.js";
  script.integrity = "sha384-WmdflGW9aGfoBdHc4rRyWzYuAjEmDwMdGdiPNacbwfGKxBW/SO6guzuQ76qjnSlr";
  script.crossOrigin = "anonymous";
  script.onload = function () {
    var nodes = [];
    blocks.forEach(function (code) {
      var div = document.createElement("div");
      div.className = "mermaid";
      div.textContent = code.textContent;
      code.parentElement.replaceWith(div);
      nodes.push(div);
    });
    var dark = /\b(navy|coal|ayu)\b/.test(document.documentElement.className);
    window.mermaid.initialize({ startOnLoad: false, theme: dark ? "dark" : "default", securityLevel: "strict" });
    window.mermaid.run({ nodes: nodes });
  };
  document.head.appendChild(script);
})();
