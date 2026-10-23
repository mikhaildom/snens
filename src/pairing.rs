// Код пейринга: короткое приглашение вместо передачи постоянного токена.
//
// Почему не «QR с токеном»: токен — постоянный секрет, и QR с ним попадает в
// историю камеры, скриншоты и буфер обмена. Код живёт 5 минут, одноразовый, и
// после обмена превращается в отдельный токен этого устройства (см. `clients`).
//
// Защита от перебора: 6 цифр — это миллион вариантов, поэтому попытки считаются:
// после `MAX_ATTEMPTS` неудач код сгорает целиком, а не дешевеет по одной.

use std::sync::Mutex;

use crate::R;

/// Сколько минут живёт код. Пять — как у телевизоров и мессенджеров: человек
/// успевает дойти до телефона, а подобрать за это время нечего.
pub const CODE_TTL_SECS: u64 = 300;
/// Попыток ввода на один код. После исчерпания код сгорает.
pub const MAX_ATTEMPTS: u32 = 5;

/// Действующее приглашение. Одно на инстанс: второе открытие пейринга заменяет
/// первое (человек передумал, старое уже не показывается на экране).
#[derive(Clone, Debug)]
pub struct Invite {
    pub code: String,
    pub created_at: u64,
    pub attempts: u32,
    /// Кто пригласил: `api`, `cli` — для строки в логе, когда их станет несколько.
    #[allow(dead_code)]
    pub origin: String,
}

impl Invite {
    pub fn expired(&self, now: u64) -> bool {
        now.saturating_sub(self.created_at) >= CODE_TTL_SECS
    }

    /// Сколько секунд осталось: клиент показывает таймер.
    pub fn left(&self, now: u64) -> u64 {
        CODE_TTL_SECS.saturating_sub(now.saturating_sub(self.created_at))
    }
}

/// Хранилище приглашения. Отдельный мьютекс: проверка кода приходит из сети, а
/// не из обработчика хода, и не должна стоять в очереди за store.
#[derive(Default)]
pub struct Pending {
    inner: Mutex<Option<Invite>>,
}

impl Pending {
    /// Открыть новое приглашение, заменив прежнее.
    pub fn open(&self, origin: &str) -> R<Invite> {
        let invite = Invite {
            code: generate(),
            created_at: super::clients::now(),
            attempts: 0,
            origin: origin.to_string(),
        };
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "хранилище приглашений отравлено")?;
        *guard = Some(invite.clone());
        Ok(invite)
    }

    pub fn current(&self) -> Option<Invite> {
        self.inner.lock().ok().and_then(|g| g.clone())
    }

    /// Проверить и погасить код. `Ok(())` — можно выдавать токен.
    ///
    /// Ошибки текстом, потому что их читает человек на экране клиента: «код
    /// истёк» отличается от «неверный код», и это разные действия.
    pub fn claim(&self, code: &str) -> Result<(), &'static str> {
        let mut guard = self.inner.lock().map_err(|_| "хранилище приглашений отравлено")?;
        let Some(invite) = guard.as_mut() else {
            return Err("пейринг не открыт: на ядре выполни `snens pair`");
        };
        if invite.expired(super::clients::now()) {
            *guard = None;
            return Err("код истёк: на ядре выполни `snens pair` ещё раз");
        }
        if invite.attempts >= MAX_ATTEMPTS {
            *guard = None;
            return Err("попытки исчерпаны: на ядре выполни `snens pair` ещё раз");
        }
        if !same_code(&invite.code, code) {
            invite.attempts += 1;
            if invite.attempts >= MAX_ATTEMPTS {
                *guard = None;
                return Err("код неверный, попытки исчерпаны");
            }
            return Err("код неверный");
        }
        // Успех: код одноразовый.
        *guard = None;
        Ok(())
    }
}

/// Сравнение кода: цифры и дефисы/пробелы не различаем — человек переписывает
/// код с экрана и легко ставит дефис там, где его не было.
fn same_code(expected: &str, given: &str) -> bool {
    let norm = |s: &str| -> String { s.chars().filter(char::is_ascii_digit).collect() };
    let a = norm(expected);
    let b = norm(given);
    !a.is_empty() && a == b
}

/// Шесть цифр из `uuid` — энтропии хватает (модуль 1e6 от 122 случайных бит),
/// а зависимость лишняя не нужна.
fn generate() -> String {
    let raw = uuid::Uuid::new_v4().as_u128();
    let n = (raw % 1_000_000) as u32;
    // Группами по три: код читают с экрана телефона на экран сервера.
    // Обе группы дополняются нулями: без `{:03}` у первой код выходил
    // короче (`44-987` на 44987) и не совпадал с обещанным форматом.
    format!("{:03}-{:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::now;

    fn with_invite() -> Pending {
        let p = Pending::default();
        p.open("test").unwrap();
        p
    }

    #[test]
    fn right_code_is_one_time() {
        let p = with_invite();
        let code = p.current().unwrap().code;
        assert!(p.claim(&code).is_ok());
        // Код погашен: повторный обмен невозможен.
        assert!(p.claim(&code).is_err());
        assert!(p.current().is_none());
    }

    #[test]
    fn code_is_forgiving_about_separators() {
        let p = with_invite();
        let code = p.current().unwrap().code;
        // Человек мог набрать без дефиса или с пробелом.
        let squashed = code.replace('-', "");
        assert!(p.claim(&format!(" {squashed} ")).is_ok());
    }

    #[test]
    fn wrong_code_burns_after_max_attempts() {
        let p = with_invite();
        let real = p.current().unwrap().code.clone();
        for _ in 0..MAX_ATTEMPTS {
            // Заведомо не тот код: цифры настоящего, сдвинутые на единицу.
            let wrong: String = real
                .chars()
                .map(|c| match c {
                    '0'..='8' => char::from(c as u8 + 1),
                    '9' => '0',
                    other => other,
                })
                .collect();
            assert!(p.claim(&wrong).is_err());
        }
        // После исчерпания даже правильный код не принимается: приглашение сгорело.
        assert!(p.claim(&real).is_err());
        assert!(p.current().is_none());
    }

    #[test]
    fn expired_code_is_refused() {
        let p = Pending::default();
        let mut invite = p.open("test").unwrap();
        invite.created_at = now().saturating_sub(CODE_TTL_SECS + 1);
        // Подменяем состояние целиком: проверяем именно правило срока.
        *p.inner.lock().unwrap() = Some(invite.clone());
        assert!(invite.expired(now()));
        assert_eq!(invite.left(now()), 0);
        assert!(p.claim(&invite.code).unwrap_err().contains("истёк"));
    }

    #[test]
    fn claim_without_open_pairing_explains_what_to_do() {
        let p = Pending::default();
        assert!(p.claim("123-456").unwrap_err().contains("snens pair"));
    }

    #[test]
    fn codes_have_six_digits_in_two_groups() {
        for _ in 0..50 {
            let code = generate();
            let digits: String = code.chars().filter(char::is_ascii_digit).collect();
            assert_eq!(digits.len(), 6, "код не шестизначный: {code}");
            assert_eq!(code.len(), 7, "формат код: {code}");
        }
    }
}
