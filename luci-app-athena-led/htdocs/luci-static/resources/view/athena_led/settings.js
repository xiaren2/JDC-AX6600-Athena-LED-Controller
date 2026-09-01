'use strict';
'require form';
'require fs';
'require ui';

return L.view.extend({
	load: function() {
		// 如果配置文件不存在或没有 general section，自动生成默认配置
		return fs.exec('/bin/sh', ['-c',
			'if ! uci -q get athena_led.general > /dev/null 2>&1; then ' +
			'uci set athena_led.general=settings; ' +
			'uci set athena_led.general.enabled=0; ' +
			'uci set athena_led.general.duration=5; ' +
			'uci set athena_led.general.light_level=5; ' +
			'uci set athena_led.general.display_order="banner timeBlink weather cpu mem"; ' +
			'uci set athena_led.general.net_interface=br-lan; ' +
			'uci set athena_led.general.wan_ip_custom_url="http://checkip.amazonaws.com"; ' +
			'uci set athena_led.general.custom_content="Roc-Gateway"; ' +
			'uci set athena_led.general.weather_city=Shenzhen; ' +
			'uci set athena_led.general.weather_source=wttr; ' +
			'uci set athena_led.general.weather_format=simple; ' +
			'uci set athena_led.general.temp_sensors="0 1 2 3 4"; ' +
			'uci set athena_led.general.enable_sleep=0; ' +
			'uci set athena_led.general.http_length=15; ' +
			'uci set athena_led.general.cache_ttl=1800; ' +
			'uci set athena_led.general.button_gpio=71; ' +
			'uci set athena_led.general.enable_mesh_button=0; ' +
			'uci set athena_led.general.mesh_button_gpio=72; ' +
			'uci set athena_led.general.mesh_short_action=none; ' +
			'uci set athena_led.general.mesh_long_action=none; ' +
			'uci set athena_led.general.disable_led_clock=0; ' +
			'uci set athena_led.general.disable_led_medal=0; ' +
			'uci set athena_led.general.disable_led_up=0; ' +
			'uci set athena_led.general.disable_led_down=0; ' +
			'uci commit athena_led; ' +
			'fi'
		]).catch(function() {});
	},

	render: function() {
		var m, s, o;
		var self = this;

		// ========== 运行状态区域 ==========
		var statusFieldset = E('fieldset', { 'class': 'cbi-section' }, [
			E('legend', {}, _('Running Status')),
			E('div', { 'class': 'cbi-section-descr' }, [
				E('div', { 'id': 'athena_status_text' }, [
					E('em', {}, _('Collecting data...'))
				])
			])
		]);

		// 状态轮询函数
		var checkStatus = function() {
			return fs.exec('/bin/sh', ['-c', 'pidof athena-led 2>/dev/null']).then(function(res) {
				var pid = (res.stdout || '').trim().split(/\s+/)[0];
				var el = document.getElementById('athena_status_text');
				if (!el) return;
				if (pid) {
					el.innerHTML = '<span style="color:green;font-weight:bold;">' + _('RUNNING') + '</span> | PID: ' + pid;
				} else {
					el.innerHTML = '<span style="color:red;font-weight:bold;">' + _('NOT RUNNING') + '</span>';
				}
			}).catch(function() {
				var el = document.getElementById('athena_status_text');
				if (el) el.innerHTML = '<span style="color:gray;"><em>' + _('Unknown Error') + '</em></span>';
			});
		};

		checkStatus();
		self.pollFn = setInterval(checkStatus, 3000);

		// ========== 表单 ==========
		m = new form.Map('athena_led',
			_('Athena LED Controller'),
			_('JDCloud AX6600 LED Screen Ctrl')
		);

		s = m.section(form.NamedSection, 'general', 'settings');
		s.anonymous = true;
		s.addremove = false;

		// Tabs
		s.tab('general', _('General Settings'));
		s.tab('network', _('Network Settings'));
		s.tab('sensor', _('Sensor & Weather'));
		s.tab('custom', _('Custom Content'));
		s.tab('sleep', _('Scheduled Sleep'));
		s.tab('button', _('Button Settings'));
		s.tab('led', _('LED Indicators'));
		s.tab('service', _('Service Control'));

		// ================= GENERAL =================
		o = s.taboption('general', form.Flag, 'enabled', _('Enabled'));
		o.rmempty = false;

		o = s.taboption('general', form.ListValue, 'light_level', _('Brightness Level'));
		o.default = '5';
		for (var i = 0; i <= 7; i++) o.value(String(i));
		o.description = _('Adjust brightness (0-7).');

		o = s.taboption('general', form.Value, 'duration', _('Loop Interval (s)'));
		o.datatype = 'uinteger';
		o.default = '5';
		o.description = _('Time in seconds to display each module.');

		o = s.taboption('general', form.DynamicList, 'display_order', _('Display Order & Modules'));
		o.description = _('Add modules and drag to reorder.');
		o.value('year', _('Year (YYYY)'));
		o.value('date', _('Date (MM-DD)'));
		o.value('time', _('Time (HH:MM)'));
		o.value('timeBlink', _('Time (Blink)'));
		o.value('uptime', _('System Uptime'));
		o.value('weather', _('Weather'));
		o.value('cpu', _('CPU Load'));
		o.value('mem', _('RAM Usage'));
		o.value('temp', _('Temperatures'));
		o.value('ip', _('WAN IP'));
		o.value('dev', _('Online Devices (ARP)'));
		o.value('netspeed_down', _('Realtime Speed (RX)'));
		o.value('netspeed_up', _('Realtime Speed (TX)'));
		o.value('traffic_down', _('Total Traffic (RX)'));
		o.value('traffic_up', _('Total Traffic (TX)'));
		o.value('banner', _('Custom Text'));
		o.value('http_custom', _('HTTP Request Result'));

		// ================= NETWORK =================
		o = s.taboption('network', form.Value, 'net_interface', _('Network Interface'));
		o.default = 'br-lan';
		o.description = _('Interface for traffic monitoring (e.g. br-lan).');
		// 读取 /proc/net/dev 获取可用接口列表
		fs.read('/proc/net/dev').then(function(content) {
			var lines = (content || '').split('\n');
			for (var i = 2; i < lines.length; i++) {
				var name = lines[i].split(':')[0].trim();
				if (name && name !== 'lo') {
					o.value(name);
				}
			}
		}).catch(function() {});

		o = s.taboption('network', form.Value, 'wan_ip_custom_url', _('WAN IP API'));
		o.description = _('Select a preset or enter custom URL.');
		o.value('http://checkip.amazonaws.com', 'Amazon AWS');
		o.value('http://ifconfig.me/ip', 'ifconfig.me');
		o.value('http://ipv4.icanhazip.com', 'icanhazip.com');
		o.default = 'http://checkip.amazonaws.com';

		// ================= SENSOR =================
		o = s.taboption('sensor', form.MultiValue, 'temp_sensors', _('Temperature Sensors'));
		o.widget = 'checkbox';
		o.value('0', 'nss-top');
		o.value('1', 'nss');
		o.value('2', 'wcss-phya0');
		o.value('3', 'wcss-phya1');
		o.value('4', 'cpu');
		o.value('5', 'lpass');
		o.value('6', 'ddrss');
		o.description = _('Select sensors to cycle through.');

		o = s.taboption('sensor', form.ListValue, 'weather_source', _('Weather Source'));
		o.value('wttr', 'Wttr.in');
		o.value('openmeteo', 'Open-Meteo');
		o.value('seniverse', 'Seniverse');
		o.value('uapis', 'Uapis.cn');
		o.default = 'wttr';

		o = s.taboption('sensor', form.Value, 'weather_city', _('City Name'));
		o.default = 'Shenzhen';
		o.description = _('Pinyin or English.');

		o = s.taboption('sensor', form.Value, 'seniverse_key', _('Seniverse API Key'));
		o.depends('weather_source', 'seniverse');

		o = s.taboption('sensor', form.ListValue, 'weather_format', _('Weather Format'));
		o.value('simple', _('Simple (Icon + Temp)'));
		o.value('full', _('Full (Original)'));

		o = s.taboption('sensor', form.Value, 'cache_ttl', _('Cache TTL (seconds)'));
		o.datatype = 'uinteger';
		o.default = '1800';
		o.description = _('How often to refresh weather/IP/HTTP cache in seconds. Set higher (e.g. 900/1800/3600) to reduce API requests and avoid being rate-limited.');

		// ================= CUSTOM =================
		o = s.taboption('custom', form.Value, 'custom_content', _('Custom Text'));
		o.placeholder = 'Roc-Gateway';
		o.description = _('Effective only when \'Custom Text\' is added to Display Order.');

		o = s.taboption('custom', form.Value, 'http_url', _('HTTP Request URL'));
		o.placeholder = 'http://192.168.1.1/api/status';
		o.description = _('Effective only when \'HTTP Request Result\' is added to Display Order.');

		o = s.taboption('custom', form.Value, 'http_length', _('HTTP Max Length'));
		o.datatype = 'uinteger';
		o.default = '15';
		o.description = _('Max characters to display (defaults to 15). Set higher for longer text.');

		// ================= SLEEP =================
		o = s.taboption('sleep', form.Flag, 'enable_sleep', _('Enable Scheduled Sleep'));

		o = s.taboption('sleep', form.Value, 'off_time', _('Screen Off Time'));
		o.depends('enable_sleep', '1');
		o.placeholder = '23:00';
		o.description = _('HH:MM format (e.g. 23:00).');

		o = s.taboption('sleep', form.Value, 'on_time', _('Screen On Time'));
		o.depends('enable_sleep', '1');
		o.placeholder = '07:00';
		o.description = _('HH:MM format (e.g. 07:00).');

		// ================= BUTTON =================
		o = s.taboption('button', form.Value, 'button_gpio', _('Screen Button GPIO Pin'));
		o.datatype = 'uinteger';
		o.default = '71';
		o.description = _('GPIO pin offset for the screen button (default 71 for AX6600). May differ on other firmware.');

		o = s.taboption('button', form.Flag, 'enable_mesh_button', _('Enable Mesh Button'));
		o.description = _('Enable custom action mapping for the Mesh button.');

		o = s.taboption('button', form.Value, 'mesh_button_gpio', _('Mesh Button GPIO Pin'));
		o.datatype = 'uinteger';
		o.default = '72';
		o.depends('enable_mesh_button', '1');
		o.description = _('GPIO pin offset for the Mesh button (default 72 for AX6600).');

		o = s.taboption('button', form.ListValue, 'mesh_short_action', _('Mesh Short Press Action'));
		o.depends('enable_mesh_button', '1');
		o.default = 'none';
		o.value('none', _('None'));
		o.value('reboot', _('Reboot Router'));
		o.value('restart_network', _('Restart Network'));
		o.value('restart_wifi', _('Restart Wi-Fi'));
		o.value('restart_athena', _('Restart Athena LED'));

		o = s.taboption('button', form.ListValue, 'mesh_long_action', _('Mesh Long Press Action'));
		o.depends('enable_mesh_button', '1');
		o.default = 'none';
		o.value('none', _('None'));
		o.value('reboot', _('Reboot Router'));
		o.value('restart_network', _('Restart Network'));
		o.value('restart_wifi', _('Restart Wi-Fi'));
		o.value('restart_athena', _('Restart Athena LED'));

		// ================= LED INDICATORS =================
		o = s.taboption('led', form.Flag, 'disable_led_clock', _('Disable Clock LED (Status 1)'));
		o.default = '0';
		o.description = _('Turn off the small clock indicator LED to the right of the digits.');

		o = s.taboption('led', form.Flag, 'disable_led_medal', _('Disable Medal LED (Status 2)'));
		o.default = '0';
		o.description = _('Turn off the medal/connectivity indicator LED.');

		o = s.taboption('led', form.Flag, 'disable_led_up', _('Disable Up Arrow LED (Status 4)'));
		o.default = '0';
		o.description = _('Turn off the upload speed up-arrow indicator LED.');

		o = s.taboption('led', form.Flag, 'disable_led_down', _('Disable Down Arrow LED (Status 8)'));
		o.default = '0';
		o.description = _('Turn off the download speed down-arrow indicator LED.');

		// ================= SERVICE =================
		var btn_restart = s.taboption('service', form.Button, '_restart', _('Restart Service'));
		btn_restart.inputstyle = 'apply';
		btn_restart.onclick = function(ev, section_id) {
			return fs.exec('/etc/init.d/athena_led', ['restart']).then(function() {
				ui.addNotification(null, E('p', {}, _('Service restarted successfully.')));
				checkStatus();
			}).catch(function(err) {
				ui.addNotification(null, E('p', {}, _('Failed to restart: ') + err));
			});
		};

		var btn_stop = s.taboption('service', form.Button, '_stop', _('Stop Service'));
		btn_stop.inputstyle = 'remove';
		btn_stop.onclick = function(ev, section_id) {
			return fs.exec('/etc/init.d/athena_led', ['stop']).then(function() {
				ui.addNotification(null, E('p', {}, _('Service stopped.')));
				checkStatus();
			}).catch(function(err) {
				ui.addNotification(null, E('p', {}, _('Failed to stop: ') + err));
			});
		};

		// 渲染表单，并在前面插入状态区域
		return m.render().then(function(formNode) {
			var container = E('div', {});
			container.appendChild(statusFieldset);
			container.appendChild(formNode);
			return container;
		});
	},

	// 视图销毁时清除轮询定时器
	handleSaveApply: function(ev, mode) {
		if (this.pollFn) {
			clearInterval(this.pollFn);
			this.pollFn = null;
		}
		return this.super('handleSaveApply', [ev, mode]);
	},

	handleSave: function(ev) {
		return this.super('handleSave', [ev]);
	},

	unload: function() {
		if (this.pollFn) {
			clearInterval(this.pollFn);
			this.pollFn = null;
		}
	}
});
