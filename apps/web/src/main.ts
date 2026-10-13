import { createApp } from "vue";
import "@fontsource-variable/instrument-sans";
import "@fontsource-variable/jetbrains-mono";
import "./features/theme";
import "./features/appearance";
import App from "./App.vue";
import { installMenuDismissal } from "./features/dismiss-menus";
import { installContextMenuPolicy } from "./features/native-context-menu";
import { router } from "./router";

installMenuDismissal();
installContextMenuPolicy();
createApp(App).use(router).mount("#root");
