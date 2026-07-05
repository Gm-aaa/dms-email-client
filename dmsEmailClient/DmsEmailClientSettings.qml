import QtQuick
import QtQuick.Controls
import Quickshell
import Quickshell.Io
import qs.Common
import qs.Widgets
import qs.Modules.Plugins
import qs.Services

PluginSettings {
    id: root
    pluginId: "dmsEmailClient"

    // 默认按名字从 PATH 查找 dms-email-client（与 Widget 保持一致）。
    readonly property string binPath: "dms-email-client"

    // ── Account data ──
    property var accounts: []
    // 本地缓存上限（存于 config.toml，影响守护进程）
    property int cacheLimit: 50
    // 邮件正文磁盘缓存目录
    property string cacheDir: ""

    Component.onCompleted: {
        loadProcess.running = true;
    }

    // Load accounts via CLI
    Process {
        id: loadProcess
        command: [root.binPath, "config", "show"]
        running: false

        stdout: StdioCollector {
            onStreamFinished: {
                try {
                    let data = JSON.parse(this.text);
                    root.accounts = data.accounts || [];
                    root.cacheLimit = (data.cache_limit !== undefined) ? data.cache_limit : 50;
                    root.cacheDir = data.cache_dir || "";
                } catch(e) {
                    root.accounts = [];
                }
            }
        }
    }

    // Save accounts via CLI
    Process {
        id: saveProcess
        running: false
        property string payload: ""
        command: ["/usr/bin/env", "bash", "-c",
            "echo " + "'" + payload.replace(/'/g, "'\\''") + "'" +
            " | '" + root.binPath + "' config save"
        ]
    }

    function saveAccounts() {
        let json = JSON.stringify({ cache_limit: root.cacheLimit, cache_dir: root.cacheDir, accounts: root.accounts });
        saveProcess.payload = json;
        saveProcess.running = true;
    }

    function removeAccount(index) {
        let arr = root.accounts.slice();
        arr.splice(index, 1);
        root.accounts = arr;
        saveAccounts();
    }

    function toggleAccount(index) {
        let arr = root.accounts.slice();
        arr[index] = Object.assign({}, arr[index], { enabled: !arr[index].enabled });
        root.accounts = arr;
        saveAccounts();
    }

    // ── Common IMAP provider presets ──
    // hint 说明各家"密码"实际应填什么（多数需独立授权码/应用专用密码）
    readonly property var presets: [
        { label: "QQ 邮箱", name: "QQ 邮箱", host: "imap.qq.com", port: 993, ssl: true, hint: "QQ 邮箱密码请填【授权码】（在 QQ 邮箱设置 → 账户中开启 IMAP 并生成）。" },
        { label: "Gmail", name: "Gmail", host: "imap.gmail.com", port: 993, ssl: true, hint: "Gmail 需开启两步验证后使用【应用专用密码】，而非账户密码。" },
        { label: "Outlook", name: "Outlook", host: "outlook.office365.com", port: 993, ssl: true, hint: "Outlook/Microsoft 365：若开启了两步验证，请使用应用密码。" },
        { label: "网易 163", name: "163 邮箱", host: "imap.163.com", port: 993, ssl: true, hint: "163 邮箱密码请填【客户端授权码】（在网页端设置中开启 IMAP 获取）。" }
    ]

    property string presetHint: ""

    function applyPreset(p) {
        hostField.text = p.host;
        portField.text = String(p.port);
        sslToggle.checked = p.ssl;
        if (nameField.text.trim() === "")
            nameField.text = p.name;
        root.presetHint = p.hint;
    }

    function addAccount() {
        if (nameField.text.trim() === "" || hostField.text.trim() === "" || userField.text.trim() === "") return;
        let arr = root.accounts.slice();
        arr.push({
            name: nameField.text.trim(),
            host: hostField.text.trim(),
            port: parseInt(portField.text) || 993,
            username: userField.text.trim(),
            // 去掉密码/授权码中的所有空白（Gmail 应用专用密码常带空格，粘贴时一并清掉）
            password: passField.text.replace(/\s+/g, ""),
            ssl: sslToggle.checked,
            enabled: true
        });
        root.accounts = arr;
        saveAccounts();
        nameField.text = "";
        hostField.text = "";
        portField.text = "993";
        userField.text = "";
        passField.text = "";
        sslToggle.checked = true;
        root.presetHint = "";
    }

    // ── Title ──
    StyledText {
        width: parent.width
        text: "DMS Email Client 设置"
        font.pixelSize: Theme.fontSizeLarge
        font.weight: Font.Bold
        color: Theme.surfaceText
    }

    StyledText {
        width: parent.width
        text: "配置邮件客户端的显示数量和邮箱账户。新邮件由守护进程实时推送，无需设置刷新间隔。"
        font.pixelSize: Theme.fontSizeSmall
        color: Theme.surfaceVariantText
        wrapMode: Text.WordWrap
    }

    // ── Slider Settings ──
    SliderSetting {
        settingKey: "maxMailsShown"
        label: "最大显示邮件数"
        description: "弹出面板列表中显示的最大邮件数量（列表可滚动）"
        defaultValue: 15
        minimum: 1
        maximum: 60
    }

    SliderSetting {
        settingKey: "popoutHeight"
        label: "下拉栏高度"
        description: "点击图标后弹出面板的高度"
        defaultValue: 380
        minimum: 240
        maximum: 720
        unit: "px"
    }

    // ── Local cache limit (stored in config.toml, affects daemon) ──
    Column {
        width: parent.width
        spacing: Theme.spacingS

        StyledText {
            text: "本地缓存上限"
            font.pixelSize: Theme.fontSizeMedium
            font.weight: Font.Medium
            color: Theme.surfaceText
        }
        StyledText {
            width: parent.width
            text: "缓存的未读邮件总数上限，在已启用账户间平均分配（如 2 个账户 + 60 = 每个 30）"
            font.pixelSize: Theme.fontSizeSmall
            color: Theme.surfaceVariantText
            wrapMode: Text.WordWrap
        }
        DankSlider {
            width: parent.width
            value: root.cacheLimit
            minimum: 10
            maximum: 200
            unit: "封"
            wheelEnabled: false
            onSliderValueChanged: newValue => {
                if (newValue !== root.cacheLimit) {
                    root.cacheLimit = newValue;
                    root.saveAccounts();
                }
            }
        }
    }

    // ── Cache directory (stored in config.toml) ──
    Column {
        width: parent.width
        spacing: Theme.spacingS

        StyledText {
            text: "缓存目录"
            font.pixelSize: Theme.fontSizeMedium
            font.weight: Font.Medium
            color: Theme.surfaceText
        }
        StyledText {
            width: parent.width
            text: "邮件正文的磁盘缓存目录（已打开过的邮件再次查看时直接读本地，不再联网）"
            font.pixelSize: Theme.fontSizeSmall
            color: Theme.surfaceVariantText
            wrapMode: Text.WordWrap
        }
        DankTextField {
            id: cacheDirField
            width: parent.width
            text: root.cacheDir
            placeholderText: "~/.cache/dms-email-client"
            onEditingFinished: {
                let v = text.trim();
                if (v !== root.cacheDir) {
                    root.cacheDir = v;
                    root.saveAccounts();
                }
            }
        }
    }

    // ── Translation Section ──
    StyledText {
        width: parent.width
        text: "翻译设置"
        font.pixelSize: Theme.fontSizeMedium
        font.weight: Font.Bold
        color: Theme.surfaceText
        topPadding: Theme.spacingM
    }

    ToggleSetting {
        settingKey: "translateEnabled"
        label: "启用翻译"
        description: "在邮件详情页显示翻译按钮。可选在线引擎（快）或本地离线模型。"
        defaultValue: false
    }

    SelectionSetting {
        settingKey: "translateEngine"
        label: "翻译引擎"
        description: "Google / DeepLX：联网、秒级、质量好，但正文会发送到外部服务。本地 NLLB：完全离线、隐私，但 CPU 上较慢（首次需联网下载约 600MB 模型到 ~/.local/share/dms-email-client/models）。"
        options: [
            { label: "Google 翻译（在线）", value: "google" },
            { label: "DeepLX（在线，自托管）", value: "deeplx" },
            { label: "本地 NLLB（离线）", value: "nllb" }
        ]
        defaultValue: "google"
    }

    SelectionSetting {
        settingKey: "translateSourceLang"
        label: "源语言"
        description: "邮件原文语言，通常保持自动检测即可。"
        options: [
            { label: "自动检测", value: "auto" },
            { label: "英语", value: "eng_Latn" },
            { label: "俄语", value: "rus_Cyrl" },
            { label: "日语", value: "jpn_Jpan" },
            { label: "韩语", value: "kor_Hang" },
            { label: "法语", value: "fra_Latn" },
            { label: "德语", value: "deu_Latn" },
            { label: "西班牙语", value: "spa_Latn" }
        ]
        defaultValue: "auto"
    }

    SelectionSetting {
        settingKey: "translateTargetLang"
        label: "目标语言"
        options: [
            { label: "简体中文", value: "zho_Hans" },
            { label: "英语", value: "eng_Latn" },
            { label: "日语", value: "jpn_Jpan" }
        ]
        defaultValue: "zho_Hans"
    }

    StringSetting {
        settingKey: "deeplxUrl"
        label: "DeepLX 地址"
        description: "仅当引擎选 DeepLX 时使用。填你自托管/公共 DeepLX 的完整 translate 接口地址（key 若有可直接放在路径里）。"
        placeholder: "https://api.deeplx.org/<key>/translate"
        defaultValue: ""
    }

    // ── Account Management Section ──
    StyledText {
        width: parent.width
        text: "邮箱账户"
        font.pixelSize: Theme.fontSizeMedium
        font.weight: Font.Bold
        color: Theme.surfaceText
        topPadding: Theme.spacingM
    }

    // Account list
    Column {
        width: parent.width
        spacing: Theme.spacingS

        Repeater {
            model: root.accounts
            delegate: StyledRect {
                required property var modelData
                required property int index
                width: parent.width
                height: accountContent.implicitHeight + Theme.spacingM * 2
                color: Theme.surfaceContainer
                radius: Theme.cornerRadius

                Row {
                    id: accountContent
                    anchors.fill: parent
                    anchors.margins: Theme.spacingM
                    spacing: Theme.spacingM

                    // Account info
                    Column {
                        spacing: 2
                        width: parent.width - actionRow.width - Theme.spacingM
                        anchors.verticalCenter: parent.verticalCenter

                        StyledText {
                            text: modelData.name || "未命名账户"
                            font.weight: Font.Bold
                            font.pixelSize: Theme.fontSizeMedium
                            color: modelData.enabled ? Theme.surfaceText : Theme.surfaceVariantText
                            elide: Text.ElideRight
                            width: parent.width
                        }
                        StyledText {
                            text: (modelData.host || "") + ":" + (modelData.port || 993)
                            font.pixelSize: Theme.fontSizeSmall
                            color: Theme.surfaceVariantText
                            elide: Text.ElideRight
                            width: parent.width
                        }
                        StyledText {
                            text: modelData.username || ""
                            font.pixelSize: Theme.fontSizeSmall
                            color: Theme.surfaceVariantText
                            elide: Text.ElideRight
                            width: parent.width
                        }
                    }

                    // Action buttons
                    Row {
                        id: actionRow
                        spacing: Theme.spacingM
                        anchors.verticalCenter: parent.verticalCenter

                        // Enable/disable this account
                        DankToggle {
                            anchors.verticalCenter: parent.verticalCenter
                            hideText: true
                            checked: modelData.enabled
                            onToggled: root.toggleAccount(index)
                        }

                        // Delete button
                        DankButton {
                            anchors.verticalCenter: parent.verticalCenter
                            text: "删除"
                            backgroundColor: Theme.error
                            textColor: Theme.onPrimary
                            buttonHeight: 32
                            onClicked: root.removeAccount(index)
                        }
                    }
                }
            }
        }

        // Empty state
        StyledText {
            visible: root.accounts.length === 0
            text: "暂无已配置的账户"
            font.pixelSize: Theme.fontSizeSmall
            color: Theme.surfaceVariantText
            width: parent.width
            horizontalAlignment: Text.AlignHCenter
            topPadding: Theme.spacingM
            bottomPadding: Theme.spacingM
        }
    }

    // ── Add Account Form ──
    StyledRect {
        width: parent.width
        height: addFormColumn.implicitHeight + Theme.spacingM * 2
        color: Theme.surfaceContainer
        radius: Theme.cornerRadius

        Column {
            id: addFormColumn
            anchors.fill: parent
            anchors.margins: Theme.spacingM
            spacing: Theme.spacingS

            StyledText {
                text: "添加账户"
                font.weight: Font.Bold
                font.pixelSize: Theme.fontSizeMedium
                color: Theme.surfaceText
            }

            // ── Quick presets ──
            StyledText {
                text: "快速预设"
                font.pixelSize: Theme.fontSizeSmall
                color: Theme.surfaceVariantText
            }

            Flow {
                width: parent.width
                spacing: Theme.spacingS

                Repeater {
                    model: root.presets
                    delegate: DankButton {
                        required property var modelData
                        text: modelData.label
                        buttonHeight: 32
                        backgroundColor: Theme.surfaceContainerHigh
                        textColor: Theme.surfaceText
                        onClicked: root.applyPreset(modelData)
                    }
                }
            }

            // Provider-specific hint (password type, etc.)
            StyledText {
                width: parent.width
                visible: root.presetHint !== ""
                text: root.presetHint
                font.pixelSize: Theme.fontSizeSmall
                color: Theme.warning
                wrapMode: Text.WordWrap
            }

            DankTextField {
                id: nameField
                width: parent.width
                placeholderText: "账户名称"
            }

            Row {
                width: parent.width
                spacing: Theme.spacingS

                DankTextField {
                    id: hostField
                    width: parent.width - portField.width - Theme.spacingS
                    placeholderText: "IMAP 服务器"
                }

                DankTextField {
                    id: portField
                    width: 80
                    placeholderText: "端口"
                    text: "993"
                }
            }

            DankTextField {
                id: userField
                width: parent.width
                placeholderText: "用户名"
            }

            DankTextField {
                id: passField
                width: parent.width
                placeholderText: "密码 / 授权码"
                echoMode: TextInput.Password
            }

            DankToggle {
                id: sslToggle
                width: parent.width
                text: "使用 SSL/TLS (993)"
                checked: true
            }

            DankButton {
                text: "添加"
                onClicked: root.addAccount()
                anchors.right: parent.right
            }
        }
    }
}
