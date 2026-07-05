import QtQuick
import Quickshell
import Quickshell.Io
import qs.Common
import qs.Services
import qs.Widgets
import qs.Modules.Plugins

PluginComponent {
    id: root

    // Settings from pluginData
    readonly property int maxMailsShown: (pluginData && pluginData.maxMailsShown) ? pluginData.maxMailsShown : 15

    // Popout configurations
    popoutWidth: 320
    popoutHeight: (pluginData && pluginData.popoutHeight) ? pluginData.popoutHeight : 380

    // Rust 二进制（守护进程 + CLI）。默认按名字从 PATH 查找（需先 `cargo install --path .`
    // 或把 dms-email-client 放进 PATH）。如需指定绝对路径，改这一处即可。
    readonly property string binPath: "dms-email-client"

    // Data states
    // unreadMails 现在同时包含已读与未读邮件（已读的 seen=true，仅不显示红点）
    property var unreadMails: []
    // 邮件总数（含已读）→ 控制列表是否显示
    readonly property int totalCount: root.unreadMails.length
    // 真正未读的数量（seen=false）→ 角标、标题、一键已读按钮
    readonly property int unreadCount: {
        let n = 0;
        for (let i = 0; i < root.unreadMails.length; i++)
            if (!root.unreadMails[i].seen) n++;
        return n;
    }
    property string lastUpdate: ""
    property string errorMessage: ""

    // Account filter for the list ("" = all accounts)
    property string accountFilter: ""
    readonly property var accountNames: {
        let s = [];
        for (let i = 0; i < root.unreadMails.length; i++) {
            let a = root.unreadMails[i].account;
            if (a && s.indexOf(a) < 0)
                s.push(a);
        }
        return s;
    }
    readonly property var filteredMails: root.accountFilter === ""
        ? root.unreadMails
        : root.unreadMails.filter(function(m) { return m.account === root.accountFilter; })

    // 数据变化后，若当前筛选的账户已没有未读邮件（如刚被一键已读），自动回到「全部」，
    // 否则筛选 chip 在只剩一个账户时会隐藏，导致列表看起来空了。
    onUnreadMailsChanged: {
        if (root.accountFilter !== "" && root.accountNames.indexOf(root.accountFilter) < 0)
            root.accountFilter = "";
    }

    // Detail-view state (set when a mail is opened)
    property var selectedMail: null
    property bool detailLoading: false
    property string detailError: ""
    property string detailFrom: ""
    property string detailSubject: ""
    property string detailDate: ""
    property string detailBody: ""

    // Translation state (Task 7; enabled by pluginData.translateEnabled from Task 8)
    readonly property bool translateEnabled: (pluginData && pluginData.translateEnabled) === true
    readonly property string translateSourceLang: (pluginData && pluginData.translateSourceLang) ? pluginData.translateSourceLang : "auto"
    readonly property string translateTargetLang: (pluginData && pluginData.translateTargetLang) ? pluginData.translateTargetLang : "zho_Hans"
    // 翻译引擎：google（默认）/ deeplx / nllb（本地离线）
    readonly property string translateEngine: (pluginData && pluginData.translateEngine) ? pluginData.translateEngine : "google"
    readonly property string deeplxUrl: (pluginData && pluginData.deeplxUrl) ? pluginData.deeplxUrl : ""
    property string detailBodyZh: ""      // 译文 HTML（空=未翻译）
    property bool showTranslated: false   // 当前是否显示译文
    property bool translating: false

    function openMail(mail) {
        root.selectedMail = mail;
        root.detailLoading = true;
        root.detailError = "";
        root.detailFrom = mail.from || "";
        root.detailSubject = mail.subject || "";
        root.detailDate = mail.date || "";
        root.detailBody = "";
        root.detailBodyZh = "";
        root.showTranslated = false;
        root.translating = false;
        bodyProcess.aAccount = mail.account;
        bodyProcess.aFolder = mail.folder || "INBOX";
        bodyProcess.aUid = String(mail.uid);
        bodyProcess.running = true;
        // 打开即已读：乐观去掉红点，并异步通知守护进程设置 \Seen（不离开详情页）
        root.markMailRead(mail);
    }

    function closeMail() {
        root.selectedMail = null;
    }

    // 把一封邮件标为已读：乐观更新（保留在列表，仅去红点）+ 异步 IMAP \Seen。
    // 已读则直接跳过，避免重复联网。
    function markMailRead(mail) {
        if (!mail || mail.seen)
            return;
        let uid = mail.uid;
        let acc = mail.account;
        let fld = mail.folder || "INBOX";
        root.unreadMails = root.unreadMails.map(function(m) {
            if (m.uid === uid && m.account === acc && (m.folder || "INBOX") === fld)
                return Object.assign({}, m, { seen: true });
            return m;
        });
        if (root.selectedMail && root.selectedMail.uid === uid
            && root.selectedMail.account === acc
            && (root.selectedMail.folder || "INBOX") === fld)
            root.selectedMail = Object.assign({}, root.selectedMail, { seen: true });
        readProcess.aAccount = acc;
        readProcess.aFolder = fld;
        readProcess.aUid = String(uid);
        readProcess.running = false;
        readProcess.running = true;
    }

    // 一键已读：把当前筛选（accountFilter，空=全部）下的未读全部标为已读
    function markAllRead() {
        if (root.unreadCount === 0)
            return;
        readAllProcess.aAccount = root.accountFilter;
        readAllProcess.running = false;
        readAllProcess.running = true;
    }

    // ── 轻量剪贴板提示（在弹出面板内短暂显示） ──
    property string toastText: ""
    Timer {
        id: toastTimer
        interval: 1600
        onTriggered: root.toastText = ""
    }
    function showToast(msg) {
        root.toastText = msg;
        toastTimer.restart();
    }

    function copyToClipboard(text) {
        clipProcess.payload = text;
        clipProcess.running = false;
        clipProcess.running = true;
    }
    function openExternal(url) {
        openProcess.url = url;
        openProcess.running = false;
        openProcess.running = true;
    }

    // Fetch the full body of a selected mail
    Process {
        id: bodyProcess
        property string aAccount: ""
        property string aFolder: ""
        property string aUid: ""
        command: [root.binPath, "body", aAccount, aFolder, aUid]
        running: false
        stdout: StdioCollector {
            onStreamFinished: {
                root.detailLoading = false;
                try {
                    let d = JSON.parse(this.text);
                    if (d.ok) {
                        root.detailFrom = d.from || root.detailFrom;
                        root.detailSubject = d.subject || root.detailSubject;
                        root.detailDate = d.date || root.detailDate;
                        root.detailBody = d.body || "(无正文)";
                        root.detailError = "";
                    } else {
                        root.detailError = d.error || "加载失败";
                    }
                } catch (e) {
                    root.detailError = "解析失败";
                }
            }
        }
    }

    // Translate the full body of the currently-open mail (Task 7; requires Task 6 daemon support)
    Process {
        id: translateProcess
        property string aAccount: ""
        property string aFolder: ""
        property string aUid: ""
        command: [root.binPath, "translate", aAccount, aFolder, aUid,
                  root.translateSourceLang, root.translateTargetLang,
                  root.translateEngine, root.deeplxUrl]
        running: false
        stdout: StdioCollector {
            onStreamFinished: {
                // 丢弃用户已切走的那封邮件的迟到响应，避免把 A 的译文贴到 B 上
                if (!root.selectedMail
                    || translateProcess.aAccount !== root.selectedMail.account
                    || translateProcess.aFolder !== (root.selectedMail.folder || "INBOX")
                    || translateProcess.aUid !== String(root.selectedMail.uid)) {
                    return;
                }
                root.translating = false;
                try {
                    let d = JSON.parse(this.text);
                    if (d.ok) {
                        root.detailBodyZh = d.body || "";
                        root.showTranslated = true;
                    } else {
                        root.showToast("翻译失败：" + (d.error || ""));
                    }
                } catch (e) {
                    root.showToast("翻译失败");
                }
            }
        }
    }

    // Mark a selected mail as read
    Process {
        id: readProcess
        property string aAccount: ""
        property string aFolder: ""
        property string aUid: ""
        command: [root.binPath, "read", aAccount, aFolder, aUid]
        running: false
        stdout: StdioCollector {
            onStreamFinished: {
                // 已读已在 markMailRead 中乐观更新；daemon 标记成功后会通过 watch
                // 主动推送权威状态，无需前端回查。
                try {
                    JSON.parse(this.text);
                } catch (e) {}
            }
        }
    }

    // Mark all currently-listed mails (optionally filtered by account) as read
    Process {
        id: readAllProcess
        property string aAccount: ""
        command: [root.binPath, "read-all", aAccount]
        running: false
        stdout: StdioCollector {
            onStreamFinished: {
                try {
                    let d = JSON.parse(this.text);
                    if (d.ok) {
                        // 乐观地把已标记账户的邮件全部置为已读（保留在列表，仅去红点），再回查同步
                        let acc = readAllProcess.aAccount;
                        root.unreadMails = root.unreadMails.map(function(m) {
                            if (acc === "" || m.account === acc)
                                return Object.assign({}, m, { seen: true });
                            return m;
                        });
                        root.showToast("已全部标记已读（" + (d.marked || 0) + " 封）");
                        // daemon 会通过 watch 推送权威状态，无需回查。
                    } else {
                        root.showToast("标记失败：" + (d.error || ""));
                    }
                } catch (e) {}
            }
        }
    }

    // 复制到剪贴板（wl-copy）
    Process {
        id: clipProcess
        property string payload: ""
        command: ["wl-copy", payload]
        running: false
        onRunningChanged: {
            if (!running && payload !== "")
                root.showToast("已复制：" + payload);
        }
    }

    // 用默认程序打开链接（xdg-open）
    Process {
        id: openProcess
        property string url: ""
        command: ["xdg-open", url]
        running: false
    }

    // ── Daemon lifecycle ──
    // The daemon runs for as long as this widget (i.e. the enabled plugin) lives.
    // Quickshell terminates the process when the component is destroyed, so the
    // daemon is automatically started when the plugin is enabled and stopped when
    // it is disabled/removed. The watch subscription below restarts it if it dies.
    Process {
        id: daemonProcess
        command: [root.binPath, "daemon"]
        running: false
    }

    Component.onCompleted: {
        daemonProcess.running = true;
        watchProcess.running = true;
    }

    // Reconnect backoff counter for the watch subscription.
    property int _watchRetries: 0

    Timer {
        id: watchRestartTimer
        repeat: false
        onTriggered: watchProcess.running = true
    }

    // Persistent subscription to daemon state. The daemon PUSHES one JSON line per
    // state change (new mail / read / connection error), so there is NO polling and
    // new mail reaches the UI with zero delay. When the connection drops (daemon
    // down, crash, or config-change restart) the process exits: we (re)start the
    // daemon and reconnect with exponential backoff.
    Process {
        id: watchProcess
        command: [root.binPath, "watch"]
        running: false

        stdout: SplitParser {
            onRead: line => {
                const l = line.trim();
                if (!l)
                    return;
                try {
                    let data = JSON.parse(l);
                    if (data.error) {
                        // CLI-level error (e.g. daemon not running). Do NOT reset the
                        // backoff here, so a persistently-down daemon backs off.
                        root.errorMessage = data.error;
                        root.unreadMails = [];
                    } else {
                        // A real state push: connection is healthy, reset backoff.
                        root._watchRetries = 0;
                        root.unreadMails = data.unread_mails || [];
                        root.lastUpdate = data.last_update || "";
                        let errs = data.errors || [];
                        root.errorMessage = errs.length > 0
                            ? errs.map(function(e) { return e.account + "：" + e.message; }).join("\n")
                            : "";
                    }
                } catch (e) {
                    root.errorMessage = "Parse Error";
                }
            }
        }

        onExited: exitCode => {
            // Connection ended. Self-heal: (re)start the daemon if it is not running
            // (first launch, a crash, or a config-change restart), then reconnect
            // with exponential backoff (0.5s → capped at 30s).
            if (!daemonProcess.running)
                daemonProcess.running = true;
            root._watchRetries = Math.min(root._watchRetries + 1, 6);
            watchRestartTimer.interval = Math.min(30000, 500 * Math.pow(2, root._watchRetries - 1));
            watchRestartTimer.start();
        }
    }

    // Horizontal bar component
    // NOTE: no MouseArea here — BasePill handles clicks (opens the popout).
    // Adding our own MouseArea would sit on top of BasePill's handler and
    // swallow the click, so the popout would never open.
    horizontalBarPill: Component {
        Row {
            id: contentRow
            spacing: Theme.spacingXS
            opacity: root.errorMessage ? 0.5 : 1.0

            // Mail icon
            DankIcon {
                name: root.unreadCount > 0 ? "mail" : "mail_outline"
                size: Theme.iconSizeSmall
                color: root.unreadCount > 0 ? Theme.primary : Theme.surfaceText
                anchors.verticalCenter: parent.verticalCenter
            }

            // Unread Count tag
            Rectangle {
                visible: root.unreadCount > 0
                width: Math.max(16, countText.implicitWidth + 8)
                height: 16
                radius: 8
                color: Theme.primary
                anchors.verticalCenter: parent.verticalCenter

                StyledText {
                    id: countText
                    text: root.unreadCount > 99 ? "99+" : root.unreadCount.toString()
                    font.pixelSize: 10
                    font.weight: Font.Bold
                    color: Theme.onPrimary
                    anchors.centerIn: parent
                }
            }

            // Error indicator
            DankIcon {
                visible: root.errorMessage !== ""
                name: "error"
                size: Theme.iconSizeSmall - 4
                color: Theme.error
                anchors.verticalCenter: parent.verticalCenter
            }
        }
    }

    // Vertical bar component
    verticalBarPill: Component {
        Column {
            id: contentColumn
            spacing: Theme.spacingXS
            opacity: root.errorMessage ? 0.5 : 1.0

            DankIcon {
                name: root.unreadCount > 0 ? "mail" : "mail_outline"
                size: Theme.iconSizeSmall
                color: root.unreadCount > 0 ? Theme.primary : Theme.surfaceText
                anchors.horizontalCenter: parent.horizontalCenter
            }

            Rectangle {
                visible: root.unreadCount > 0
                width: Math.max(16, vCountText.implicitWidth + 8)
                height: 16
                radius: 8
                color: Theme.primary
                anchors.horizontalCenter: parent.horizontalCenter

                StyledText {
                    id: vCountText
                    text: root.unreadCount > 99 ? "99+" : root.unreadCount.toString()
                    font.pixelSize: 10
                    font.weight: Font.Bold
                    color: Theme.onPrimary
                    anchors.centerIn: parent
                }
            }
        }
    }

    // Popout details window
    popoutContent: Component {
        PopoutComponent {
            id: popout
            headerText: root.selectedMail ? "邮件详情" : "未读邮件"
            showCloseButton: false

            // ─────────────── List view ───────────────
            Column {
                width: parent.width
                spacing: Theme.spacingM
                visible: root.selectedMail === null

                // Header status block (left: status, right: 一键已读 + 刷新)
                StyledRect {
                    width: parent.width
                    height: 50
                    color: Theme.surfaceContainerHigh
                    radius: Theme.cornerRadius

                    // Left: icon + status text
                    Row {
                        anchors.left: parent.left
                        anchors.right: headerActions.left
                        anchors.verticalCenter: parent.verticalCenter
                        anchors.leftMargin: Theme.spacingM
                        anchors.rightMargin: Theme.spacingS
                        spacing: Theme.spacingS

                        DankIcon {
                            name: "mail"
                            size: Theme.iconSize
                            color: root.unreadCount > 0 ? Theme.primary : Theme.surfaceVariantText
                            anchors.verticalCenter: parent.verticalCenter
                        }

                        StyledText {
                            text: root.unreadCount > 0
                                ? ("您有 " + root.unreadCount + " 封未读邮件")
                                : (root.errorMessage ? "连接错误"
                                    : (root.totalCount > 0 ? "邮件已全部读完" : "暂无邮件"))
                            font.weight: Font.Bold
                            font.pixelSize: Theme.fontSizeMedium
                            color: Theme.surfaceText
                            elide: Text.ElideRight
                            anchors.verticalCenter: parent.verticalCenter
                        }
                    }

                    // Right: action buttons
                    Row {
                        id: headerActions
                        anchors.right: parent.right
                        anchors.verticalCenter: parent.verticalCenter
                        anchors.rightMargin: Theme.spacingS
                        spacing: Theme.spacingXS

                        // 一键已读：标记当前筛选下全部未读为已读
                        Rectangle {
                            width: markAllRow.implicitWidth + Theme.spacingM
                            height: Theme.iconSize * 1.4
                            radius: Theme.cornerRadius
                            visible: root.unreadCount > 0
                            color: markAllArea.containsMouse ? Theme.primaryHover : Theme.primary
                            anchors.verticalCenter: parent.verticalCenter

                            Row {
                                id: markAllRow
                                anchors.centerIn: parent
                                spacing: Theme.spacingXS
                                DankIcon {
                                    id: markAllIcon
                                    // 进行中：换成转圈图标并持续旋转
                                    name: readAllProcess.running ? "autorenew" : "mark_email_read"
                                    size: Theme.iconSize * 0.7
                                    color: Theme.onPrimary
                                    anchors.verticalCenter: parent.verticalCenter
                                    RotationAnimator {
                                        target: markAllIcon
                                        from: 0; to: 360
                                        duration: 800
                                        loops: Animation.Infinite
                                        running: readAllProcess.running
                                    }
                                    Connections {
                                        target: readAllProcess
                                        function onRunningChanged() {
                                            if (!readAllProcess.running)
                                                markAllIcon.rotation = 0;
                                        }
                                    }
                                }
                                StyledText {
                                    text: readAllProcess.running ? "处理中…" : "一键已读"
                                    font.pixelSize: Theme.fontSizeSmall
                                    color: Theme.onPrimary
                                    anchors.verticalCenter: parent.verticalCenter
                                }
                            }
                            MouseArea {
                                id: markAllArea
                                anchors.fill: parent
                                hoverEnabled: true
                                cursorShape: Qt.PointingHandCursor
                                // 进行中忽略点击，避免重复触发
                                onClicked: if (!readAllProcess.running) root.markAllRead()
                            }
                        }

                        // 刷新
                        Rectangle {
                            width: Theme.iconSize * 1.4
                            height: Theme.iconSize * 1.4
                            radius: Theme.cornerRadius
                            color: refreshArea.containsMouse ? Theme.surfaceContainerHighest : Theme.surfaceContainer
                            anchors.verticalCenter: parent.verticalCenter

                            DankIcon {
                                id: refreshIcon
                                anchors.centerIn: parent
                                name: "refresh"
                                size: Theme.iconSize * 0.8
                                color: refreshArea.containsMouse ? Theme.primary : Theme.surfaceText
                                // 刷新近乎瞬时（只读守护进程内存状态），用一次性 360° 转动作为点击反馈
                                RotationAnimator {
                                    id: refreshSpin
                                    target: refreshIcon
                                    from: 0; to: 360
                                    duration: 500
                                    running: false
                                }
                            }
                            MouseArea {
                                id: refreshArea
                                anchors.fill: parent
                                hoverEnabled: true
                                cursorShape: Qt.PointingHandCursor
                                onClicked: {
                                    refreshSpin.restart();
                                    // 状态本就由 daemon 实时推送；手动刷新即断开重连一次
                                    // watch，强制立刻重新拉取一份权威状态（onExited 负责重连）。
                                    watchProcess.running = false;
                                }
                            }
                        }
                    }
                }

                // 操作提示（如「已全部标记已读」）
                StyledText {
                    visible: root.toastText !== ""
                    width: parent.width
                    text: root.toastText
                    font.pixelSize: Theme.fontSizeSmall
                    color: Theme.primary
                    horizontalAlignment: Text.AlignHCenter
                    wrapMode: Text.WordWrap
                }

                // Error message display (grows with content)
                StyledRect {
                    visible: root.errorMessage !== ""
                    width: parent.width
                    height: errorText.implicitHeight + Theme.spacingM * 2
                    color: Theme.surfaceContainerHigh
                    radius: Theme.cornerRadius

                    StyledText {
                        id: errorText
                        anchors.verticalCenter: parent.verticalCenter
                        anchors.horizontalCenter: parent.horizontalCenter
                        text: root.errorMessage
                        color: Theme.error
                        font.pixelSize: Theme.fontSizeSmall
                        wrapMode: Text.WordWrap
                        width: parent.width - Theme.spacingM * 2
                    }
                }

                // Account filter chips (only when more than one account has mail)
                Flow {
                    width: parent.width
                    spacing: Theme.spacingXS
                    visible: root.accountNames.length > 1

                    // "全部" chip
                    Rectangle {
                        height: 24
                        width: allChipText.implicitWidth + Theme.spacingM
                        radius: 12
                        color: root.accountFilter === "" ? Theme.primary : Theme.surfaceContainerHigh
                        StyledText {
                            id: allChipText
                            anchors.centerIn: parent
                            text: "全部"
                            font.pixelSize: Theme.fontSizeSmall
                            color: root.accountFilter === "" ? Theme.onPrimary : Theme.surfaceText
                        }
                        MouseArea {
                            anchors.fill: parent
                            cursorShape: Qt.PointingHandCursor
                            onClicked: root.accountFilter = ""
                        }
                    }

                    Repeater {
                        model: root.accountNames
                        delegate: Rectangle {
                            required property var modelData
                            readonly property bool active: root.accountFilter === modelData
                            height: 24
                            width: chipText.implicitWidth + Theme.spacingM
                            radius: 12
                            color: active ? Theme.primary : Theme.surfaceContainerHigh
                            StyledText {
                                id: chipText
                                anchors.centerIn: parent
                                text: modelData
                                font.pixelSize: Theme.fontSizeSmall
                                color: active ? Theme.onPrimary : Theme.surfaceText
                            }
                            MouseArea {
                                anchors.fill: parent
                                cursorShape: Qt.PointingHandCursor
                                onClicked: root.accountFilter = modelData
                            }
                        }
                    }
                }

                // Mail list (scrollable)
                DankFlickable {
                    width: parent.width
                    height: Math.max(80, root.popoutHeight - (root.accountNames.length > 1 ? 175 : 145))
                    visible: root.totalCount > 0
                    contentHeight: mailColumn.implicitHeight
                    clip: true

                    Column {
                        id: mailColumn
                        width: parent.width
                        spacing: Theme.spacingS

                        Repeater {
                            model: root.filteredMails.slice(0, root.maxMailsShown)
                            delegate: StyledRect {
                                required property var modelData
                                width: mailColumn.width
                                height: delcol.implicitHeight + Theme.spacingM * 2
                                color: mailArea.containsMouse ? Theme.surfaceContainerHigh : Theme.surfaceContainer
                                radius: Theme.cornerRadius
                                // 已读邮件暗化（仍保留显示）
                                opacity: modelData.seen ? 0.55 : 1.0

                                readonly property bool isSpam: modelData.folder && modelData.folder !== "INBOX"

                                Column {
                                    id: delcol
                                    anchors.left: parent.left
                                    anchors.right: parent.right
                                    anchors.verticalCenter: parent.verticalCenter
                                    anchors.margins: Theme.spacingM
                                    anchors.leftMargin: Theme.spacingM
                                    anchors.rightMargin: Theme.spacingM
                                    spacing: 3

                                    Row {
                                        width: parent.width
                                        spacing: Theme.spacingXS
                                        // 未读红点（已读时透明，保留占位以对齐）
                                        Rectangle {
                                            width: 8
                                            height: 8
                                            radius: 4
                                            color: modelData.seen ? "transparent" : Theme.error
                                            anchors.verticalCenter: parent.verticalCenter
                                        }
                                        StyledText {
                                            text: modelData.from || "未知发件人"
                                            font.weight: modelData.seen ? Font.Normal : Font.Bold
                                            font.pixelSize: Theme.fontSizeSmall
                                            color: Theme.surfaceText
                                            elide: Text.ElideRight
                                            width: parent.width - 56 - 8 - Theme.spacingXS * 2
                                            anchors.verticalCenter: parent.verticalCenter
                                        }
                                        StyledText {
                                            text: modelData.date ? modelData.date.substring(11, 16) : ""
                                            font.pixelSize: 10
                                            color: Theme.surfaceVariantText
                                            horizontalAlignment: Text.AlignRight
                                            width: 56
                                            anchors.verticalCenter: parent.verticalCenter
                                        }
                                    }

                                    StyledText {
                                        text: modelData.subject || "(无主题)"
                                        font.pixelSize: 11
                                        color: Theme.surfaceVariantText
                                        elide: Text.ElideRight
                                        width: parent.width
                                    }

                                    // Category tags: account + folder
                                    Row {
                                        spacing: Theme.spacingXS

                                        Rectangle {
                                            height: 16
                                            width: accTag.implicitWidth + 10
                                            radius: 8
                                            color: Theme.surfaceContainerHighest
                                            StyledText {
                                                id: accTag
                                                anchors.centerIn: parent
                                                text: modelData.account || ""
                                                font.pixelSize: 9
                                                color: Theme.surfaceVariantText
                                            }
                                        }

                                        Rectangle {
                                            height: 16
                                            width: folderTag.implicitWidth + 10
                                            radius: 8
                                            color: isSpam ? Theme.error : Theme.primary
                                            StyledText {
                                                id: folderTag
                                                anchors.centerIn: parent
                                                text: isSpam ? "垃圾邮件" : "收件箱"
                                                font.pixelSize: 9
                                                color: isSpam ? Theme.surfaceText : Theme.onPrimary
                                            }
                                        }
                                    }
                                }

                                MouseArea {
                                    id: mailArea
                                    anchors.fill: parent
                                    hoverEnabled: true
                                    cursorShape: Qt.PointingHandCursor
                                    onClicked: root.openMail(modelData)
                                }
                            }
                        }
                    }
                }

                // Empty state indicator
                StyledRect {
                    visible: root.totalCount === 0 && root.errorMessage === ""
                    width: parent.width
                    height: 100
                    color: Theme.surfaceContainer
                    radius: Theme.cornerRadius

                    Column {
                        anchors.centerIn: parent
                        spacing: Theme.spacingS
                        DankIcon {
                            name: "mail_outline"
                            size: Theme.iconSize * 1.5
                            color: Theme.surfaceVariantText
                            anchors.horizontalCenter: parent.horizontalCenter
                        }
                        StyledText {
                            text: "收件箱已全部读完"
                            font.pixelSize: Theme.fontSizeSmall
                            color: Theme.surfaceVariantText
                            anchors.horizontalCenter: parent.horizontalCenter
                        }
                    }
                }

            }

            // ─────────────── Detail view ───────────────
            Column {
                width: parent.width
                spacing: Theme.spacingS
                visible: root.selectedMail !== null

                // Toolbar: back（打开邮件已自动标记已读，无需手动按钮）
                Row {
                    width: parent.width
                    spacing: Theme.spacingS

                    Rectangle {
                        width: Theme.iconSize * 1.4
                        height: Theme.iconSize * 1.4
                        radius: Theme.cornerRadius
                        color: backArea.containsMouse ? Theme.surfaceContainerHighest : Theme.surfaceContainerHigh
                        DankIcon {
                            anchors.centerIn: parent
                            name: "arrow_back"
                            size: Theme.iconSize * 0.8
                            color: Theme.surfaceText
                        }
                        MouseArea {
                            id: backArea
                            anchors.fill: parent
                            hoverEnabled: true
                            cursorShape: Qt.PointingHandCursor
                            onClicked: root.closeMail()
                        }
                    }
                }

                // Subject
                StyledText {
                    width: parent.width
                    text: root.detailSubject || "(无主题)"
                    font.weight: Font.Bold
                    font.pixelSize: Theme.fontSizeMedium
                    color: Theme.surfaceText
                    wrapMode: Text.WordWrap
                }

                // From + date
                StyledText {
                    width: parent.width
                    text: root.detailFrom
                    font.pixelSize: Theme.fontSizeSmall
                    color: Theme.surfaceVariantText
                    wrapMode: Text.WordWrap
                }
                StyledText {
                    width: parent.width
                    text: root.detailDate
                    font.pixelSize: 10
                    color: Theme.surfaceVariantText
                }

                Rectangle {
                    width: parent.width
                    height: 1
                    color: Theme.outline
                    opacity: 0.3
                }

                // Loading / error / body
                StyledText {
                    visible: root.detailLoading
                    text: "正在加载正文…"
                    font.pixelSize: Theme.fontSizeSmall
                    color: Theme.surfaceVariantText
                }
                StyledText {
                    visible: root.detailError !== ""
                    width: parent.width
                    text: "加载失败：" + root.detailError
                    font.pixelSize: Theme.fontSizeSmall
                    color: Theme.error
                    wrapMode: Text.WordWrap
                }

                // 翻译按钮：仅在设置中启用且正文已加载成功时显示（Task 8 提供 translateEnabled 开关）
                Rectangle {
                    visible: root.translateEnabled && !root.detailLoading && root.detailError === ""
                    width: tLabel.implicitWidth + Theme.spacingM * 2
                    height: Theme.iconSize * 1.4
                    radius: Theme.cornerRadius
                    // 译文态：填充主色更醒目；原文态：淡容器底、悬停提亮
                    color: root.showTranslated
                           ? Theme.primary
                           : (tArea.containsMouse ? Theme.surfaceContainerHighest : Theme.surfaceContainer)
                    StyledText {
                        id: tLabel
                        anchors.centerIn: parent
                        text: root.translating
                              ? (root.translateEngine === "nllb" ? "翻译中…（本地模型，首次较慢）" : "翻译中…")
                              : (root.showTranslated ? "原文" : "翻译")
                        font.pixelSize: Theme.fontSizeSmall
                        // 原文态用主色文字让按钮更突出；译文态在填充底上用 onPrimary
                        color: root.showTranslated ? Theme.onPrimary : Theme.primary
                    }
                    MouseArea {
                        id: tArea
                        anchors.fill: parent
                        hoverEnabled: true
                        cursorShape: Qt.PointingHandCursor
                        onClicked: {
                            if (root.translating) return;
                            if (root.showTranslated) { root.showTranslated = false; return; }
                            if (root.detailBodyZh !== "") { root.showTranslated = true; return; }
                            // 尚未翻译：触发 daemon
                            root.translating = true;
                            translateProcess.aAccount = root.selectedMail.account;
                            translateProcess.aFolder = root.selectedMail.folder || "INBOX";
                            translateProcess.aUid = String(root.selectedMail.uid);
                            translateProcess.running = false;
                            translateProcess.running = true;
                        }
                    }
                }

                DankFlickable {
                    visible: !root.detailLoading && root.detailError === ""
                    width: parent.width
                    height: Math.max(100, root.popoutHeight - 230)
                    contentHeight: bodyText.implicitHeight
                    clip: true

                    // 正文 HTML 由 Rust 守护进程生成（URL/验证码已是可点击链接），
                    // 这里只注入主题色后用 RichText 渲染；可鼠标选中复制。
                    TextEdit {
                        id: bodyText
                        width: parent.width
                        text: (root.showTranslated ? root.detailBodyZh : root.detailBody)
                              .replace(/<a /g, '<a style="color:' + String(Theme.primary) + ';text-decoration:none" ')
                        textFormat: TextEdit.RichText
                        readOnly: true
                        selectByMouse: true
                        persistentSelection: true
                        wrapMode: TextEdit.Wrap
                        font.pixelSize: Theme.fontSizeSmall
                        font.family: Theme.fontFamily
                        color: Theme.surfaceText
                        selectionColor: Theme.primary
                        selectedTextColor: Theme.onPrimary
                        onLinkActivated: function(link) {
                            if (link.indexOf("copy:") === 0)
                                root.copyToClipboard(link.substring(5));
                            else
                                root.openExternal(link);
                        }

                        // 链接上显示手型光标（NoButton 不抢选择/点击事件）
                        MouseArea {
                            anchors.fill: parent
                            acceptedButtons: Qt.NoButton
                            cursorShape: bodyText.hoveredLink ? Qt.PointingHandCursor : Qt.IBeamCursor
                        }
                    }
                }

                // 剪贴板/操作的轻量提示
                StyledText {
                    visible: root.toastText !== ""
                    width: parent.width
                    text: root.toastText
                    font.pixelSize: Theme.fontSizeSmall
                    color: Theme.primary
                    horizontalAlignment: Text.AlignHCenter
                    wrapMode: Text.WordWrap
                }
            }
        }
    }
}
