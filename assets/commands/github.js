// GitHub searches for the command box: "gh owner/repo" opens a repository
// or its issues, "gh owner/repo#12" an issue, and "gh some words" searches
// repositories and issues. The pages open in the browser of the agent you
// are with. Shift+Enter copies the link instead.

const base = "https://github.com";
const item = (title, url, subtitle = "") => ({ title, subtitle, glyph: "gh", action: openUrl(url), alt: copy(url) });

export default command({
  keyword: "gh",
  title: "Search GitHub",
  subtitle: "gh owner/repo, gh owner/repo#12 or gh words",
  run(query) {
    const q = query.trim();
    if (q === "") return [item("Open GitHub", base)];
    const issue = q.match(/^([\w.-]+)\/([\w.-]+)#(\d+)$/);
    if (issue) {
      const [, owner, repo, n] = issue;
      return [item(`Issue #${n} of ${owner}/${repo}`, `${base}/${owner}/${repo}/issues/${n}`)];
    }
    const repo = q.match(/^([\w.-]+)\/([\w.-]+)$/);
    if (repo) {
      return [
        item(`Open ${q}`, `${base}/${q}`),
        item(`Issues of ${q}`, `${base}/${q}/issues`),
        item(`Pull requests of ${q}`, `${base}/${q}/pulls`),
      ];
    }
    const words = encodeURIComponent(q);
    return [
      item(`Repositories about "${q}"`, `${base}/search?q=${words}&type=repositories`),
      item(`Issues about "${q}"`, `${base}/search?q=${words}&type=issues`),
    ];
  },
});
