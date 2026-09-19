//! Telex input for the on-screen keyboard: "tinhf" -> "tình", "dduwowngf" -> "đường".
//!
//! Each key is applied to the last word of the text. The word is kept as base
//! letters plus one tone, and the tone is re-placed after every change, so
//! marks can be typed anywhere in the word (like Unikey's "free marking").

/// Vowels by shape, with their tones in order: none, sắc, huyền, hỏi, ngã, nặng.
const VOWELS: [[char; 6]; 12] = [
    ['a', 'á', 'à', 'ả', 'ã', 'ạ'],
    ['ă', 'ắ', 'ằ', 'ẳ', 'ẵ', 'ặ'],
    ['â', 'ấ', 'ầ', 'ẩ', 'ẫ', 'ậ'],
    ['e', 'é', 'è', 'ẻ', 'ẽ', 'ẹ'],
    ['ê', 'ế', 'ề', 'ể', 'ễ', 'ệ'],
    ['i', 'í', 'ì', 'ỉ', 'ĩ', 'ị'],
    ['o', 'ó', 'ò', 'ỏ', 'õ', 'ọ'],
    ['ô', 'ố', 'ồ', 'ổ', 'ỗ', 'ộ'],
    ['ơ', 'ớ', 'ờ', 'ở', 'ỡ', 'ợ'],
    ['u', 'ú', 'ù', 'ủ', 'ũ', 'ụ'],
    ['ư', 'ứ', 'ừ', 'ử', 'ữ', 'ự'],
    ['y', 'ý', 'ỳ', 'ỷ', 'ỹ', 'ỵ'],
];

/// (shape, tone) of a vowel, or None for consonants.
fn split(c: char) -> Option<(char, usize)> {
    VOWELS
        .iter()
        .find_map(|row| row.iter().position(|&v| v == c).map(|t| (row[0], t)))
}

fn with_tone(shape: char, tone: usize) -> char {
    VOWELS
        .iter()
        .find(|row| row[0] == shape)
        .map(|row| row[tone])
        .unwrap_or(shape)
}

fn is_vowel(c: char) -> bool {
    split(c).is_some()
}

fn is_shaped(c: char) -> bool {
    matches!(c, 'ă' | 'â' | 'ê' | 'ô' | 'ơ' | 'ư')
}

fn tone_key(k: char) -> Option<usize> {
    match k {
        's' => Some(1),
        'f' => Some(2),
        'r' => Some(3),
        'x' => Some(4),
        'j' => Some(5),
        _ => None,
    }
}

/// Removes the tone: base letters (shapes kept) and the tone that was on them.
fn strip(word: &str) -> (Vec<char>, usize) {
    let mut tone = 0;
    let base = word
        .chars()
        .map(|c| match split(c) {
            Some((shape, t)) => {
                if t != 0 {
                    tone = t;
                }
                shape
            }
            None => c,
        })
        .collect();
    (base, tone)
}

/// The vowel nucleus [start, end): "qu" and "gi" count as consonants when a
/// vowel follows them.
fn nucleus(w: &[char]) -> (usize, usize) {
    let mut start = w.iter().position(|&c| is_vowel(c)).unwrap_or(w.len());
    if start > 0 && start + 1 < w.len() && is_vowel(w[start + 1]) {
        let onset = (w[start - 1], w[start]);
        if onset == ('q', 'u') || onset == ('g', 'i') {
            start += 1;
        }
    }
    let mut end = start;
    while end < w.len() && is_vowel(w[end]) {
        end += 1;
    }
    (start, end)
}

/// Puts `tone` on the right vowel (traditional placement: hóa, thúy, người).
fn place(base: &[char], tone: usize) -> String {
    let mut w = base.to_vec();
    let (s, e) = nucleus(&w);
    if tone != 0 && s < e {
        let pos = if let Some(p) = (s..e).rev().find(|&i| is_shaped(w[i])) {
            p
        } else if e - s == 1 {
            s
        } else if e < w.len() {
            e - 1 // closed syllable: hoán, toàn
        } else if e - s == 2 {
            s // open: hóa, múa, tái
        } else {
            s + 1 // three vowels: khoái, ngoẹo
        };
        w[pos] = with_tone(w[pos], tone);
    }
    w.into_iter().collect()
}

fn find_in(w: &[char], range: (usize, usize), c: char) -> Option<usize> {
    (range.0..range.1).rev().find(|&i| w[i] == c)
}

fn apply_word(word: &str, key: char) -> String {
    let (mut w, tone) = strip(word);
    let nuc = nucleus(&w);
    let has_vowel = nuc.0 < nuc.1;

    if let Some(t) = tone_key(key).filter(|_| has_vowel) {
        if tone == t {
            // Pressing the same tone twice types the letter instead.
            let mut s = place(&w, 0);
            s.push(key);
            return s;
        }
        return place(&w, t);
    }
    match key {
        'z' if tone != 0 => return place(&w, 0),
        'a' | 'e' | 'o' => {
            let hat = match key {
                'a' => 'â',
                'e' => 'ê',
                _ => 'ô',
            };
            if let Some(i) = find_in(&w, nuc, key) {
                w[i] = hat;
                return place(&w, tone);
            }
            if let Some(i) = find_in(&w, nuc, hat) {
                // "aa" twice: back to plain letters.
                w[i] = key;
                w.push(key);
                return place(&w, tone);
            }
        }
        'w' => {
            let (s, e) = nuc;
            let seg: String = w[s..e].iter().collect();
            let hook = |w: &mut Vec<char>, i: usize| {
                w[i] = match w[i] {
                    'a' => 'ă',
                    'o' => 'ơ',
                    'u' => 'ư',
                    c => c,
                }
            };
            if let Some(off) = seg.find("uo").or_else(|| seg.find("ưo")).or_else(|| seg.find("uơ")) {
                let i = s + seg[..off].chars().count();
                w[i] = 'ư';
                w[i + 1] = 'ơ';
                return place(&w, tone);
            }
            if seg.starts_with("oa") {
                hook(&mut w, s + 1);
                return place(&w, tone);
            }
            for target in ['u', 'o', 'a'] {
                if let Some(i) = find_in(&w, nuc, target) {
                    hook(&mut w, i);
                    return place(&w, tone);
                }
            }
            if (s..e).any(|i| matches!(w[i], 'ă' | 'ơ' | 'ư')) {
                // "ww": remove the hooks and type a plain w.
                for c in w[s..e].iter_mut() {
                    *c = match *c {
                        'ă' => 'a',
                        'ơ' => 'o',
                        'ư' => 'u',
                        c => c,
                    };
                }
                w.push('w');
                return place(&w, tone);
            }
            if !has_vowel {
                w.push('ư');
                return place(&w, tone);
            }
        }
        'd' => {
            if let Some(i) = w.iter().position(|&c| c == 'd') {
                w[i] = 'đ';
                return place(&w, tone);
            }
            if let Some(i) = w.iter().position(|&c| c == 'đ') {
                w[i] = 'd';
                w.push('d');
                return place(&w, tone);
            }
        }
        _ => {}
    }
    w.push(key);
    place(&w, tone)
}

/// Applies one key press to the last word of `text`.
pub fn apply(text: &str, key: char) -> String {
    let key = key.to_ascii_lowercase();
    let split_at = text.rfind(' ').map(|i| i + 1).unwrap_or(0);
    let (head, word) = text.split_at(split_at);
    format!("{head}{}", apply_word(word, key))
}

#[cfg(test)]
mod tests {
    use super::apply;

    fn typed(keys: &str) -> String {
        keys.chars().fold(String::new(), |t, k| {
            if k == ' ' {
                t + " "
            } else {
                apply(&t, k)
            }
        })
    }

    #[test]
    fn common_words() {
        for (keys, want) in [
            ("nhacj lanhf chuwa tinhf", "nhạc lành chưa tình"),
            ("tinhf", "tình"),
            ("dduwowngf", "đường"),
            ("nguoiwf", "người"),
            ("thuongw", "thương"),
            ("vieetj nam", "việt nam"),
            ("tuyeenr", "tuyển"),
            ("khoer", "khỏe"),
            ("hoawcj", "hoặc"),
            ("hoaf", "hòa"),
            ("hoasn", "hoán"),
            ("quas", "quá"),
            ("gias", "giá"),
            ("gif", "gì"),
            ("cuwar", "cửa"),
            ("muwa", "mưa"),
            ("tuaans", "tuấn"),
            ("khoais", "khoái"),
            ("sown tungf", "sơn tùng"),
            ("dden", "đen"),
            ("bin", "bin"),
        ] {
            assert_eq!(typed(keys), want, "keys {keys}");
        }
    }

    #[test]
    fn undo_by_repeating() {
        assert_eq!(typed("tinhff"), "tinhf");
        assert_eq!(typed("aaa"), "aa");
        assert_eq!(typed("ddd"), "dd");
        assert_eq!(typed("tinhfz"), "tinh");
        assert_eq!(typed("w"), "ư");
    }
}
