// Temperatures for the command box: "temp 21" shows 21 °C in Fahrenheit
// and Kelvin, and 21 °F in Celsius. Enter copies the number, Shift+Enter
// copies it with its unit. The API is in crates/meerora-script/API.md.

const round = (x) => String(Math.round(x * 10) / 10);

export default command({
  keyword: "temp",
  title: "Convert temperatures",
  subtitle: "temp 21",
  run(query) {
    const value = Number(query.trim().replace(",", "."));
    if (query.trim() === "" || !Number.isFinite(value)) {
      return [{ title: "Type a temperature, like temp 21", glyph: "°" }];
    }
    const f = round(value * 9 / 5 + 32);
    const k = round(value + 273.15);
    const c = round((value - 32) * 5 / 9);
    return [
      { title: `${value} °C = ${f} °F`, subtitle: `${value} °C is ${k} K`, glyph: "°", action: copy(f), alt: copy(`${f} °F`) },
      { title: `${value} °F = ${c} °C`, glyph: "°", action: copy(c), alt: copy(`${c} °C`) },
      { title: `${value} °C = ${k} K`, glyph: "K", action: copy(k), alt: copy(`${k} K`) },
    ];
  },
});
