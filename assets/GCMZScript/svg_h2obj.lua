-- GCMZDrops ハンドラー: .svg / .svgz → SVG_H
--
-- 正本: AI/plugins/svg_h/assets/GCMZScript/svg_h2obj.lua
-- 配置: Plugin/GCMZDrops/GCMZScript/svg_h2obj.lua（build.ps1 がコピーする。配置先を直接直さない）
-- 検証: python AI/plugins/svg_h/verify_svg_h2obj.py
--
-- 書き込む項目名と選択肢のラベルは svg_h の lib.rs と揃える。
-- .aup2 には項目名がキー、--select 相当の値はラベル文字列で入る（番号ではない）。
--
-- .svg / .svgz 以外は触らない。GCMZDrops は優先度の小さい順にすべてのハンドラーの drop を呼び、
-- 前のハンドラーが書き換えたファイル一覧を次へ渡す（GCMZScript/entrypoint.lua）。
-- ここで他の拡張子を .object に差し替えると、後に続くハンドラー（.txt を読む VariableFont.lua 等）や
-- GCMZDrops 本体の既定の取り込みから、そのファイルを横取りすることになる。

local P = {}

local ini = require("ini")

-- ハンドラー名（必須）
P.name = i18n({
  ja_JP = [=[SVGファイルをSVG_Hオブジェクトに変換]=],
  en_US = [=[Convert SVG files to SVG_H objects]=],
  zh_CN = [=[将SVG文件转换为SVG_H对象]=],
})

-- 優先度（省略時は 1000。小さいほど先に実行される）
-- 同じ .svg を扱う svg2obj.lua（本家 SVG、優先度 1000）より先に拾うため小さくする
P.priority = 900

-- 設定項目
P.settings = {
  use_alt_key = false, -- Altキーを押下したときのみ有効にする
}
-- 設定項目

local function file_ext(filepath)
  local ext = filepath:match("%.([^./\\]+)$")
  if ext then
    return ext:lower()
  end
  return nil
end

function P.drag_enter(files, state)
  for _, file in ipairs(files) do
    local ext = file_ext(file.filepath)
    if ext == "svg" or ext == "svgz" then
      return true
    end
  end
  return false
end

function P.drag_leave()
end

local function write_object_temp(obj, temp_name)
  local temp_path = gcmz.create_temp_file(temp_name)
  local temp_file = io.open(temp_path, "wb")
  if not temp_file then
    debug_print(i18n({
      ja_JP = [=[一時ファイルの作成に失敗しました: ]=],
      en_US = [=[Failed to create a temporary file: ]=],
      zh_CN = [=[临时文件创建失败: ]=],
    }) .. temp_path)
    return nil
  end
  temp_file:write(tostring(obj))
  temp_file:close()
  return temp_path
end

local function read_binary_file(path)
  local f = io.open(path, "rb")
  if not f then
    return nil
  end
  local data = f:read("*a")
  f:close()
  return data or ""
end

-- 一時フォルダ上の SVG を保存先へ複製する
--
-- ブラウザーや 7-Zip などからのドロップでは、GCMZDrops が中身を一時フォルダ
-- （%TEMP%\gcmzdrops<PID>\）へ書き出して渡してくる。GCMZDrops 本体は、ハンドラーを
-- 通った後のファイル一覧に残ったものだけを保存先へ複製する。ここでは SVG を .object に
-- 差し替えるので、SVG 自体は複製されず、一時フォルダのパスが .object に残る。
-- 一時フォルダは AviUtl2 の終了時に消えるため、プロジェクトを開き直すと SVG が見つからない。
--
-- GCMZDrops の既定（processing_mode = auto / direct）と同じく、一時ファイルか
-- %TMP% / %TEMP% の配下にあるものだけを複製する（auto が対象に含めるシステムフォルダーは見ない）。

local function normalize_dir(path)
  if type(path) ~= "string" or path == "" then
    return nil
  end
  path = path:gsub("/", "\\"):lower()
  if path:sub(-1) ~= "\\" then
    path = path .. "\\"
  end
  return path
end

local function is_under_temp_dir(filepath)
  local getenv = os and os.getenv
  if not getenv then
    return false
  end
  local p = filepath:gsub("/", "\\"):lower()
  for _, name in ipairs({ "TMP", "TEMP" }) do
    local dir = normalize_dir(getenv(name))
    if dir and p:sub(1, #dir) == dir then
      return true
    end
  end
  return false
end

-- 内容から 8 桁の 16 進を作る（sdbm）。保存先で同名の別ファイルを上書きしないための識別子で、
-- 同じ内容なら同じ名前になる（同じ SVG を何度落としても 1 つにまとまる）
local function content_hash(data)
  local h = 0
  local n = #data
  for i = 1, n, 4096 do
    local bytes = { data:byte(i, math.min(i + 4095, n)) }
    for j = 1, #bytes do
      h = (bytes[j] + h * 65599) % 4294967296
    end
  end
  return string.format("%04x%04x", math.floor(h / 65536), h % 65536)
end

-- 保存名は GCMZDrops 本体と同じ「名前.ハッシュ.拡張子」
local function save_file_name(file, data)
  local name = file.filepath:match("[^\\/]+$") or "drop.svg"
  local stem, ext = name:match("^(.+)%.([^.]+)$")
  if not stem then
    stem, ext = name, "svg"
  end
  if file.temporary then
    -- GCMZDrops が一時ファイル名に付ける「_16 桁の 16 進」を落とす
    stem = stem:gsub("_" .. string.rep("%x", 16) .. "$", "")
    if stem == "" then
      stem = "svg"
    end
  end
  return stem .. "." .. content_hash(data) .. "." .. ext
end

-- .object に書くパスを返す。複製できなかったときは元のパスのまま（ログに理由を出す）
local function persistent_svg_path(file)
  if not (file.temporary or is_under_temp_dir(file.filepath)) then
    return file.filepath
  end
  local data = read_binary_file(file.filepath)
  if data == nil then
    debug_print(i18n({
      ja_JP = [=[SVG ファイルを読み込めないため、一時フォルダのまま参照します: ]=],
      en_US = [=[Failed to read the SVG file; referencing the temporary path: ]=],
      zh_CN = [=[无法读取 SVG 文件，将直接引用临时路径: ]=],
    }) .. tostring(file.filepath))
    return file.filepath
  end
  local saved, err = gcmz.save_file(file.filepath, save_file_name(file, data))
  if not saved then
    debug_print(i18n({
      ja_JP = [=[SVG ファイルを保存先へ複製できなかったため、一時フォルダのまま参照します: ]=],
      en_US = [=[Failed to copy the SVG file to the save folder; referencing the temporary path: ]=],
      zh_CN = [=[无法将 SVG 文件复制到保存位置，将直接引用临时路径: ]=],
    }) .. tostring(file.filepath) .. " (" .. tostring(err) .. ")")
    return file.filepath
  end
  return saved
end

function P.drop(files, state)
  if P.settings.use_alt_key and not state.alt then
    return false
  end

  for _, file in ipairs(files) do
    local ext = file_ext(file.filepath)
    if ext == "svg" or ext == "svgz" then
      local obj = ini.new()
      obj:set("Object", "frame", "0,200")
      obj:set("Object.0", "effect.name", "SVG_H")
      obj:set("Object.0", "ファイル", tostring(persistent_svg_path(file)))
      -- ストローク拡張のデフォルト（上書きOFF = 本家互換の見た目）
      -- 選択項目は番号ではなくラベルで書く（lib.rs の #[item(name = ...)]）
      obj:set("Object.0", "塗りを上書き", "0")
      obj:set("Object.0", "ストロークを上書き", "0")
      obj:set("Object.0", "線幅", "1.00")
      obj:set("Object.0", "線種", "実線")
      obj:set("Object.0", "線種オフセット", "0.00")
      obj:set("Object.0", "端", "butt")
      obj:set("Object.0", "角", "miter")
      obj:set("Object.0", "マイター限界", "4.00")
      obj:set("Object.1", "effect.name", "標準描画")

      local temp_path = write_object_temp(obj, "svg_h2obj.object")
      if not temp_path then
        return false
      end
      file.filepath = temp_path
      file.mimetype = ""
      file.temporary = true
    end
  end
  return true
end

return P
