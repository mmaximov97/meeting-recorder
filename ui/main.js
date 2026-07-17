// Фронтенд — тонкий: всё решает аудио-поток, здесь только показ и кнопки.
// Никакой копии состояния машины тут нет и быть не должно — оно приходит
// событием "state" и только оттуда.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

const ПОДПИСЬ = {
  idle: "Ожидание встречи",
  armed: "Похоже, встреча — записать?",
  recording: "Идёт запись",
};

function показать_ошибку(текст) {
  $("err").textContent = текст ?? "";
}

// Фатальная ошибка — не «ошибка на этом тике», а «дальше ничего не работает».
// Приходит двумя путями: событием (если мы успели подписаться) и снимком из
// get_state (если не успели — а на старте не успевает никто). Пути сходятся
// здесь, и повторный показ той же причины безвреден.
function показать_фатальную(причина) {
  показать_ошибку(`${причина} — запись работать не будет`);
  $("toggle").disabled = true;
  $("yes").disabled = true;
}

async function команда(имя) {
  try {
    await invoke("send_event", { name: имя });
    показать_ошибку("");
  } catch (e) {
    показать_ошибку(String(e));
  }
}

function размер(байты) {
  if (байты < 1024) return `${байты} Б`;
  const мб = байты / (1024 * 1024);
  return мб < 1 ? `${(байты / 1024).toFixed(0)} КБ` : `${мб.toFixed(1)} МБ`;
}

function применить_состояние(s) {
  $("state").textContent = ПОДПИСЬ[s] ?? s;
  $("lamp").className = `lamp ${s}`;
  $("toggle").textContent = s === "recording" ? "Остановить запись" : "Начать запись";
  // Вопрос живёт ровно столько, сколько Armed: ушла сессия или пришёл ответ —
  // баннер обязан исчезнуть сам, иначе на экране останется вопрос, на который
  // уже некому отвечать.
  $("ask").classList.toggle("on", s === "armed");
  if (s !== "recording" && s !== "armed") обновить_список();
}

async function обновить_список() {
  try {
    const записи = await invoke("list_recordings");
    const list = $("list");
    list.innerHTML = "";
    if (записи.length === 0) {
      const li = document.createElement("li");
      li.className = "empty";
      li.textContent = "Пока пусто";
      list.append(li);
      return;
    }
    for (const з of записи) {
      const li = document.createElement("li");
      const имя = document.createElement("div");
      имя.className = "name";
      имя.textContent = з.name;
      const мета = document.createElement("div");
      мета.className = "meta";
      // Отсутствие дорожки — не косметика: пара mic+system и есть запись.
      const дорожки = [з.mic ? "mic" : null, з.system ? "system" : null].filter(Boolean);
      мета.textContent = `${дорожки.join(" + ")} · ${размер(з.size)}`;
      if (!з.mic || !з.system) {
        мета.classList.add("warn");
        мета.textContent += " · дорожка отсутствует";
      }
      li.append(имя, мета);
      list.append(li);
    }
  } catch (e) {
    показать_ошибку(String(e));
  }
}

$("yes").addEventListener("click", () => команда("confirm"));
$("no").addEventListener("click", () => команда("decline"));
$("toggle").addEventListener("click", () => команда("toggle"));
$("folder").addEventListener("click", async () => {
  try {
    await invoke("open_folder");
  } catch (e) {
    показать_ошибку(String(e));
  }
});

// Окно живёт в трее и показывается спустя часы после загрузки страницы: без
// этого список остался бы тем, каким его собрали на старте приложения.
window.addEventListener("focus", обновить_список);

// Порядок здесь — не вкусовщина. Сначала подписки, потом снимок: подписавшись
// первыми, мы не пропустим изменение, случившееся во время запроса, а снимок
// следом покажет то, что случилось ДО подписки. Наоборот — оставило бы дырку
// ровно между двумя шагами.
//
// Снимок нужен именно потому, что Tauri не буферизует события: аудио-поток
// живёт с setup(), эта страница загружается позже (а после перезагрузки окна —
// намного позже), и всё, что он успел сообщить до нас, мы можем только спросить.
async function старт() {
  await listen("state", (e) => применить_состояние(e.payload));
  await listen("ask", (e) => {
    $("ask-text").textContent = `Похоже, встреча (${e.payload}). Записать?`;
  });
  await listen("error", (e) => показать_ошибку(String(e.payload)));
  await listen("fatal", (e) => показать_фатальную(String(e.payload)));

  try {
    const снимок = await invoke("get_state");
    применить_состояние(снимок.state);
    if (снимок.fatal) показать_фатальную(снимок.fatal);
  } catch (e) {
    показать_ошибку(String(e));
  }

  // Безусловно, хотя применить_состояние выше могло уже дёрнуть список: в Armed
  // и в записи оно этого не делает, а показать прошлые записи надо в любом
  // состоянии. Лишний read_dir на старте дешевле, чем связка с чужим условием,
  // которая молча оставит список пустым, если то условие однажды поменяется.
  обновить_список();
}

старт();
