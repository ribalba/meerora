//! The calculator of the command box: arithmetic with `+ - * / ^ mod`,
//! hex, binary and octal literals, percentages, factorials, functions such
//! as `sqrt` and `sin`, constants and unit conversions like
//! `5 km in miles`. A small recursive descent parser, so nothing runs a
//! script and every input finishes at once.

/// A result of [`eval`].
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    pub value: f64,
    /// The value as shown and copied, for example `36` or `3.1068559612`.
    pub text: String,
    /// Extra forms for the subtitle, such as `0x24 · 0b100100`, or the unit.
    pub note: Option<String>,
}

/// Evaluates `input` when it looks like a calculation. Plain numbers and
/// words return None, so typing an app's name never shows a sum.
pub fn eval(input: &str) -> Option<Answer> {
    let input = input.trim();
    if input.is_empty() || input.len() > 400 {
        return None;
    }
    if let Some(answer) = convert(input) {
        return Some(answer);
    }
    let tokens = lex(input)?;
    if !worth_showing(&tokens) {
        return None;
    }
    let mut p = Parser { tokens: &tokens, at: 0 };
    let value = p.expr()?;
    if p.at != tokens.len() || !value.is_finite() {
        return None;
    }
    let radix = tokens.iter().any(|t| matches!(t, Tok::Num(_, true)));
    let note = (radix && value.fract() == 0.0 && value.abs() < 2f64.powi(53)).then(|| {
        let n = value as i64;
        let sign = if n < 0 { "-" } else { "" };
        format!("{sign}0x{:x} · {sign}0b{:b}", n.unsigned_abs(), n.unsigned_abs())
    });
    Some(Answer { value, text: format(value), note })
}

/// A calculation needs an operator, a function or a literal in another base,
/// so `42`, `pi` and app names stay out.
fn worth_showing(tokens: &[Tok]) -> bool {
    let operator = tokens.iter().any(|t| matches!(t, Tok::Op(_) | Tok::LParen));
    let radix = tokens.iter().any(|t| matches!(t, Tok::Num(_, true)));
    // `7 mod 3` and `3 x 4` have words for operators.
    let number = tokens.iter().any(|t| matches!(t, Tok::Num(..)));
    let word = tokens.iter().any(|t| matches!(t, Tok::Ident(w) if matches!(w.as_str(), "mod" | "of" | "x")));
    // `2pi` multiplies a number with a constant.
    let implicit = tokens.windows(2).any(|w| matches!((&w[0], &w[1]), (Tok::Num(..), Tok::Ident(name)) if constant(name).is_some()));
    operator || radix || (number && word) || implicit
}

/// Shows up to 10 decimals without trailing zeros, and large or tiny
/// values in scientific notation.
pub fn format(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let a = v.abs();
    if !(1e-6..1e15).contains(&a) {
        let s = format!("{v:.9e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        return format!("{mantissa}e{exp}");
    }
    if v.fract() == 0.0 {
        return format!("{v:.0}");
    }
    let s = format!("{v:.10}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// A number, and whether it was written in another base than 10.
    Num(f64, bool),
    Ident(String),
    /// `+ - * / ^ % !` and `,`.
    Op(char),
    LParen,
    RParen,
}

fn lex(input: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '0'..='9' | '.' => {
                let (num, len) = number(&chars[i..])?;
                out.push(num);
                i += len;
            }
            'a'..='z' | 'A'..='Z' | '_' | 'π' | 'τ' | 'φ' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || "πτφ".contains(chars[i])) {
                    i += 1;
                }
                out.push(Tok::Ident(chars[start..i].iter().collect::<String>().to_lowercase()));
            }
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '*' if chars.get(i + 1) == Some(&'*') => {
                out.push(Tok::Op('^'));
                i += 2;
            }
            '+' | '-' | '*' | '/' | '^' | '%' | '!' | ',' => {
                out.push(Tok::Op(c));
                i += 1;
            }
            '×' | '·' => {
                out.push(Tok::Op('*'));
                i += 1;
            }
            '÷' => {
                out.push(Tok::Op('/'));
                i += 1;
            }
            '−' => {
                out.push(Tok::Op('-'));
                i += 1;
            }
            _ => return None,
        }
    }
    Some(out)
}

/// A number at the start of `chars`: `12`, `1_000`, `1.5e3`, `0x1f`, `0b101`, `0o17`.
fn number(chars: &[char]) -> Option<(Tok, usize)> {
    if chars[0] == '0'
        && let Some(radix) = chars.get(1).and_then(|c| match c.to_ascii_lowercase() {
            'x' => Some(16),
            'b' => Some(2),
            'o' => Some(8),
            _ => None,
        })
    {
        let digits: String = chars[2..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == '_').filter(|c| **c != '_').collect();
        let len = 2 + chars[2..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == '_').count();
        let value = u64::from_str_radix(&digits, radix).ok()?;
        return Some((Tok::Num(value as f64, true), len));
    }
    let mut len = 0;
    let mut text = String::new();
    let mut seen_dot = false;
    while let Some(&c) = chars.get(len) {
        match c {
            '0'..='9' => text.push(c),
            '_' => {}
            '.' if !seen_dot => {
                seen_dot = true;
                text.push(c);
            }
            'e' | 'E'
                if chars.get(len + 1).is_some_and(|n| {
                    n.is_ascii_digit() || ((*n == '-' || *n == '+') && chars.get(len + 2).is_some_and(char::is_ascii_digit))
                }) =>
            {
                text.push('e');
                len += 1;
                if let Some(&sign) = chars.get(len).filter(|c| **c == '-' || **c == '+') {
                    text.push(sign);
                    len += 1;
                }
                while let Some(&d) = chars.get(len).filter(|c| c.is_ascii_digit()) {
                    text.push(d);
                    len += 1;
                }
                break;
            }
            _ => break,
        }
        len += 1;
    }
    let value: f64 = text.parse().ok()?;
    Some((Tok::Num(value, false), len))
}

struct Parser<'a> {
    tokens: &'a [Tok],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.at)
    }

    fn eat_op(&mut self, op: char) -> bool {
        if self.peek() == Some(&Tok::Op(op)) {
            self.at += 1;
            return true;
        }
        false
    }

    /// Takes the `)` that must come next.
    fn close_paren(&mut self) -> Option<()> {
        if self.peek() != Some(&Tok::RParen) {
            return None;
        }
        self.at += 1;
        Some(())
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Ident(w)) if w == word) {
            self.at += 1;
            return true;
        }
        false
    }

    fn expr(&mut self) -> Option<f64> {
        let (mut left, _) = self.term()?;
        loop {
            let op = if self.eat_op('+') {
                1.0
            } else if self.eat_op('-') {
                -1.0
            } else {
                return Some(left);
            };
            let (right, percent) = self.term()?;
            // `200 + 10%` is 220, as on a calculator.
            left += op * if percent { left * right } else { right };
        }
    }

    /// A product. Returns whether it is a bare percentage like `10%`.
    fn term(&mut self) -> Option<(f64, bool)> {
        let (mut left, mut percent) = self.unary()?;
        loop {
            if self.eat_op('*') || self.eat_word("of") || self.eat_word("x") {
                left *= self.unary()?.0;
            } else if self.eat_op('/') {
                left /= self.unary()?.0;
            } else if self.eat_word("mod") || self.eat_op('%') {
                left = left.rem_euclid(self.unary()?.0);
            } else if matches!(self.peek(), Some(Tok::LParen) | Some(Tok::Ident(_)))
                && !matches!(self.peek(), Some(Tok::Ident(w)) if is_keyword(w))
            {
                // `2pi` and `3(4 + 5)`.
                left *= self.unary()?.0;
            } else {
                return Some((left, percent));
            }
            percent = false;
        }
    }

    fn unary(&mut self) -> Option<(f64, bool)> {
        if self.eat_op('-') {
            let (v, p) = self.unary()?;
            return Some((-v, p));
        }
        if self.eat_op('+') {
            return self.unary();
        }
        self.power()
    }

    fn power(&mut self) -> Option<(f64, bool)> {
        let (base, percent) = self.postfix()?;
        if self.eat_op('^') {
            // Right associative, and `2^-1` works.
            let (exp, _) = self.unary()?;
            return Some((base.powf(exp), false));
        }
        Some((base, percent))
    }

    fn postfix(&mut self) -> Option<(f64, bool)> {
        let mut v = self.primary()?;
        let mut percent = false;
        loop {
            if self.eat_op('!') {
                v = factorial(v)?;
            } else if self.peek() == Some(&Tok::Op('%')) && !self.operand_follows(self.at + 1) {
                // A percent sign before an operand is `mod`, see `term`.
                self.at += 1;
                v /= 100.0;
                percent = true;
            } else {
                return Some((v, percent));
            }
        }
    }

    fn operand_follows(&self, at: usize) -> bool {
        match self.tokens.get(at) {
            Some(Tok::Num(..)) | Some(Tok::LParen) => true,
            Some(Tok::Ident(w)) => !is_keyword(w),
            _ => false,
        }
    }

    fn primary(&mut self) -> Option<f64> {
        match self.peek()?.clone() {
            Tok::Num(v, _) => {
                self.at += 1;
                Some(v)
            }
            Tok::LParen => {
                self.at += 1;
                let v = self.expr()?;
                self.close_paren()?;
                Some(v)
            }
            Tok::Ident(name) => {
                self.at += 1;
                if self.peek() == Some(&Tok::LParen) {
                    self.at += 1;
                    let mut args = Vec::new();
                    if self.peek() != Some(&Tok::RParen) {
                        args.push(self.expr()?);
                        while self.eat_op(',') {
                            args.push(self.expr()?);
                        }
                    }
                    self.close_paren()?;
                    return function(&name, &args);
                }
                constant(&name)
            }
            _ => None,
        }
    }
}

fn is_keyword(word: &str) -> bool {
    matches!(word, "of" | "mod" | "x" | "in" | "to" | "as")
}

fn constant(name: &str) -> Option<f64> {
    Some(match name {
        "pi" | "π" => std::f64::consts::PI,
        "tau" | "τ" => std::f64::consts::TAU,
        "e" => std::f64::consts::E,
        "phi" | "φ" => 1.618_033_988_749_895,
        _ => return None,
    })
}

fn function(name: &str, args: &[f64]) -> Option<f64> {
    let one = || (args.len() == 1).then(|| args[0]);
    Some(match name {
        "sqrt" => one()?.sqrt(),
        "cbrt" => one()?.cbrt(),
        "abs" => one()?.abs(),
        "ln" => one()?.ln(),
        "log" if args.len() == 2 => args[0].ln() / args[1].ln(),
        "log" | "log10" => one()?.log10(),
        "log2" => one()?.log2(),
        "exp" => one()?.exp(),
        "sin" => one()?.sin(),
        "cos" => one()?.cos(),
        "tan" => one()?.tan(),
        "asin" => one()?.asin(),
        "acos" => one()?.acos(),
        "atan" => one()?.atan(),
        "sinh" => one()?.sinh(),
        "cosh" => one()?.cosh(),
        "tanh" => one()?.tanh(),
        "floor" => one()?.floor(),
        "ceil" => one()?.ceil(),
        "round" => one()?.round(),
        "trunc" => one()?.trunc(),
        "deg" => one()?.to_degrees(),
        "rad" => one()?.to_radians(),
        "min" if !args.is_empty() => args.iter().copied().fold(f64::INFINITY, f64::min),
        "max" if !args.is_empty() => args.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        "pow" if args.len() == 2 => args[0].powf(args[1]),
        "hypot" if args.len() == 2 => args[0].hypot(args[1]),
        _ => return None,
    })
}

fn factorial(v: f64) -> Option<f64> {
    if v < 0.0 || v.fract() != 0.0 || v > 170.0 {
        return None;
    }
    Some((1..=v as u32).fold(1.0, |acc, n| acc * f64::from(n)))
}

// ------------------------------------------------------------------ units

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dim {
    Length,
    Mass,
    Time,
    Data,
    Volume,
    Speed,
    Area,
    Energy,
    Temperature,
}

/// A unit: its dimension, its factor to the base unit and its short name.
/// Temperatures use the factor as a code (0 °C, 1 °F, 2 K).
fn unit(name: &str) -> Option<(Dim, f64, &'static str)> {
    use Dim::*;
    let name = name.trim().to_lowercase();
    let name = name.trim_start_matches('°');
    Some(match name {
        "m" | "meter" | "meters" | "metre" | "metres" => (Length, 1.0, "m"),
        "km" | "kilometer" | "kilometers" | "kilometre" | "kilometres" => (Length, 1000.0, "km"),
        "cm" | "centimeter" | "centimeters" => (Length, 0.01, "cm"),
        "mm" | "millimeter" | "millimeters" => (Length, 0.001, "mm"),
        "um" | "µm" | "micrometer" | "micrometers" => (Length, 1e-6, "µm"),
        "nm" | "nanometer" | "nanometers" => (Length, 1e-9, "nm"),
        "mi" | "mile" | "miles" => (Length, 1609.344, "mi"),
        "nmi" => (Length, 1852.0, "nmi"),
        "yd" | "yard" | "yards" => (Length, 0.9144, "yd"),
        "ft" | "foot" | "feet" => (Length, 0.3048, "ft"),
        "inch" | "inches" => (Length, 0.0254, "in"),
        "kg" | "kilogram" | "kilograms" => (Mass, 1.0, "kg"),
        "g" | "gram" | "grams" => (Mass, 0.001, "g"),
        "mg" | "milligram" | "milligrams" => (Mass, 1e-6, "mg"),
        "t" | "tonne" | "tonnes" => (Mass, 1000.0, "t"),
        "lb" | "lbs" | "pound" | "pounds" => (Mass, 0.453_592_37, "lb"),
        "oz" | "ounce" | "ounces" => (Mass, 0.028_349_523_125, "oz"),
        "st" | "stone" | "stones" => (Mass, 6.350_293_18, "st"),
        "s" | "sec" | "secs" | "second" | "seconds" => (Time, 1.0, "s"),
        "ms" | "millisecond" | "milliseconds" => (Time, 0.001, "ms"),
        "min" | "mins" | "minute" | "minutes" => (Time, 60.0, "min"),
        "h" | "hr" | "hrs" | "hour" | "hours" => (Time, 3600.0, "h"),
        "d" | "day" | "days" => (Time, 86400.0, "d"),
        "week" | "weeks" => (Time, 604_800.0, "weeks"),
        "year" | "years" => (Time, 31_557_600.0, "years"),
        "byte" | "bytes" => (Data, 1.0, "bytes"),
        "bit" | "bits" => (Data, 0.125, "bits"),
        "kb" => (Data, 1e3, "kB"),
        "mb" => (Data, 1e6, "MB"),
        "gb" => (Data, 1e9, "GB"),
        "tb" => (Data, 1e12, "TB"),
        "kib" => (Data, 1024.0, "KiB"),
        "mib" => (Data, 1_048_576.0, "MiB"),
        "gib" => (Data, 1_073_741_824.0, "GiB"),
        "tib" => (Data, 1_099_511_627_776.0, "TiB"),
        "l" | "liter" | "liters" | "litre" | "litres" => (Volume, 1.0, "l"),
        "ml" | "milliliter" | "milliliters" => (Volume, 0.001, "ml"),
        "cl" => (Volume, 0.01, "cl"),
        "dl" => (Volume, 0.1, "dl"),
        "m3" => (Volume, 1000.0, "m³"),
        "gal" | "gallon" | "gallons" => (Volume, 3.785_411_784, "gal"),
        "qt" | "quart" | "quarts" => (Volume, 0.946_352_946, "qt"),
        "pt" | "pint" | "pints" => (Volume, 0.473_176_473, "pt"),
        "cup" | "cups" => (Volume, 0.236_588_236_5, "cups"),
        "floz" => (Volume, 0.029_573_529_562_5, "fl oz"),
        "m/s" | "mps" => (Speed, 1.0, "m/s"),
        "km/h" | "kmh" | "kph" => (Speed, 1.0 / 3.6, "km/h"),
        "mph" => (Speed, 0.447_04, "mph"),
        "kn" | "knot" | "knots" => (Speed, 1852.0 / 3600.0, "kn"),
        "m2" | "sqm" => (Area, 1.0, "m²"),
        "km2" => (Area, 1e6, "km²"),
        "ha" | "hectare" | "hectares" => (Area, 1e4, "ha"),
        "acre" | "acres" => (Area, 4_046.856_422_4, "acres"),
        "ft2" | "sqft" => (Area, 0.092_903_04, "ft²"),
        "j" | "joule" | "joules" => (Energy, 1.0, "J"),
        "kj" => (Energy, 1e3, "kJ"),
        "cal" => (Energy, 4.184, "cal"),
        "kcal" => (Energy, 4184.0, "kcal"),
        "wh" => (Energy, 3600.0, "Wh"),
        "kwh" => (Energy, 3.6e6, "kWh"),
        "c" | "celsius" => (Temperature, 0.0, "°C"),
        "f" | "fahrenheit" => (Temperature, 1.0, "°F"),
        "k" | "kelvin" => (Temperature, 2.0, "K"),
        _ => return None,
    })
}

/// `5 km in miles`, `100 f to c`, `(2 + 3) kg as lb`.
fn convert(input: &str) -> Option<Answer> {
    let lower = input.to_lowercase();
    let (left, target) = [" in ", " to ", " as "]
        .iter()
        .filter_map(|k| lower.rfind(k).map(|at| (at, k.len())))
        .max_by_key(|(at, _)| *at)
        .map(|(at, len)| (lower[..at].trim().to_string(), lower[at + len..].trim().to_string()))?;
    let (to_dim, to_factor, to_name) = unit(&target)?;
    // The unit is the last word of the left side, or glued to its number (`5km`).
    let split = left.rfind(|c: char| !(c.is_alphanumeric() || c == '/' || c == '°' || c == 'µ')).map_or(0, |i| i + 1);
    let (mut expr, mut from) = (left[..split].trim().to_string(), left[split..].to_string());
    if expr.is_empty() {
        let digits = from.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '_')).unwrap_or(from.len());
        expr = from[..digits].to_string();
        from = from[digits..].to_string();
    }
    let (from_dim, from_factor, from_name) = unit(&from)?;
    if from_dim != to_dim || expr.is_empty() {
        return None;
    }
    let tokens = lex(&expr)?;
    let mut p = Parser { tokens: &tokens, at: 0 };
    let value = p.expr()?;
    if p.at != tokens.len() {
        return None;
    }
    let out = if from_dim == Dim::Temperature {
        let kelvin = match from_factor as u8 {
            0 => value + 273.15,
            1 => (value - 32.0) * 5.0 / 9.0 + 273.15,
            _ => value,
        };
        match to_factor as u8 {
            0 => kelvin - 273.15,
            1 => (kelvin - 273.15) * 9.0 / 5.0 + 32.0,
            _ => kelvin,
        }
    } else {
        value * from_factor / to_factor
    };
    if !out.is_finite() {
        return None;
    }
    let text = format(out);
    Some(Answer { value: out, note: Some(format!("{} {from_name} = {text} {to_name}", format(value))), text })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(input: &str) -> f64 {
        eval(input).unwrap_or_else(|| panic!("{input:?} gives no result")).value
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * b.abs().max(1.0)
    }

    #[test]
    fn arithmetic_follows_the_usual_rules() {
        assert_eq!(v("2+2*3"), 8.0);
        assert_eq!(v("(2+2)*3"), 12.0);
        assert_eq!(v("2^3^2"), 512.0, "power is right associative");
        assert_eq!(v("-2^2"), -4.0);
        assert_eq!(v("2**10"), 1024.0);
        assert_eq!(v("7 mod 3"), 1.0);
        assert_eq!(v("7 % 3"), 1.0, "a percent sign between numbers is mod");
        assert_eq!(v("10 / 4"), 2.5);
        assert_eq!(v("5!"), 120.0);
        assert_eq!(v("3 × 4 ÷ 2 − 1"), 5.0);
        assert_eq!(v("1_000 * 3"), 3000.0);
        assert_eq!(v("1.5e3 + 1"), 1501.0);
        assert!(close(v("2pi"), std::f64::consts::TAU));
        assert_eq!(v("3(4+5)"), 27.0);
    }

    #[test]
    fn functions_constants_and_bases() {
        assert!(close(v("sqrt(2)^3"), 2.0f64.sqrt().powi(3)));
        assert_eq!(eval("sqrt(2)^3").unwrap().text, "2.8284271247");
        assert_eq!(v("0x1f + 0b101"), 36.0);
        assert_eq!(eval("0x1f + 0b101").unwrap().note.as_deref(), Some("0x24 · 0b100100"));
        assert_eq!(v("0o17"), 15.0, "a literal in another base alone is worth showing");
        assert_eq!(v("max(3, 9, 4) - min(2, 1)"), 8.0);
        assert!(close(v("log(8, 2)"), 3.0));
        assert_eq!(v("log(1000)"), 3.0);
        assert!(close(v("sin(pi/2)"), 1.0));
        assert_eq!(v("round(2.5) + floor(-1.5)"), 1.0);
    }

    #[test]
    fn percentages_work_like_a_calculator() {
        assert_eq!(v("50%"), 0.5);
        assert!(close(v("200 + 10%"), 220.0));
        assert!(close(v("200 - 10%"), 180.0));
        assert!(close(v("10% of 200"), 20.0));
        assert!(close(v("200 * 10%"), 20.0));
        assert!(close(v("(100 + 50%) * 2"), 300.0));
    }

    #[test]
    fn units_convert() {
        assert!(close(v("5 km in miles"), 3.106_855_961_185_993));
        assert!(close(v("5km to mi"), 3.106_855_961_185_993));
        assert_eq!(eval("5 km in miles").unwrap().note.as_deref(), Some("5 km = 3.1068559612 mi"));
        assert!(close(v("100 c in f"), 212.0));
        assert!(close(v("32 °F to °C"), 0.0));
        assert!(close(v("0 k in c"), -273.15));
        assert!(close(v("1 gib in mb"), 1073.741824));
        assert!(close(v("(2 + 3) kg as lb"), 11.023_113_109_243_878));
        assert!(close(v("90 min in h"), 1.5));
        assert!(eval("5 km in kg").is_none(), "different dimensions");
        assert!(eval("5 parsecs in km").is_none());
    }

    #[test]
    fn words_and_plain_numbers_are_no_calculation() {
        for input in ["", "42", "pi", "firefox", "terminal", "e", "2+", "(1+2", "foo(3)", "1/0", "sqrt(-1)", "2 ++", "ask what is 2+2", "x"]
        {
            assert_eq!(eval(input), None, "{input:?}");
        }
    }

    #[test]
    fn numbers_are_shown_short() {
        assert_eq!(format(36.0), "36");
        assert_eq!(format(0.1 + 0.2), "0.3");
        assert_eq!(format(-2.5), "-2.5");
        assert_eq!(format(1e20), "1e20");
        assert_eq!(format(1.234_567_89e-9), "1.23456789e-9");
        assert_eq!(format(2.0f64.sqrt()), "1.4142135624");
    }
}
