/// 登录页：选服务器 → 登录 / 注册。
///
/// ## 两步而不是一步
///
/// 服务器地址与账号分两步，因为它们的失败模式完全不同：地址错了是「连不上」，
/// 账号错了是「密码不对」。混在一个表单里提交，用户拿到一句「登录失败」无从下手。
///
/// ## 「未注册就自动注册」做成提示确认
///
/// 理由见 `state/auth_controller.dart` 的文件头——一句话：打错一个字母就会凭空建出
/// 一个垃圾账号，而用户会以为自己登进了老账号。
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:lumen_mobile/core/env.dart';
import 'package:lumen_mobile/state/auth_controller.dart';
import 'package:lumen_mobile/state/providers.dart';

/// 登录页。
class LoginPage extends ConsumerStatefulWidget {
  const LoginPage({super.key});

  @override
  ConsumerState<LoginPage> createState() => _LoginPageState();
}

class _LoginPageState extends ConsumerState<LoginPage> {
  final TextEditingController _server = TextEditingController();
  final TextEditingController _email = TextEditingController();
  final TextEditingController _password = TextEditingController();

  /// 地址栏的本地校验错误（与服务端返回的错误分开显示）。
  String? _serverError;

  @override
  void dispose() {
    _server.dispose();
    _email.dispose();
    _password.dispose();
    super.dispose();
  }

  void _selectServer() {
    final ServerEndpoint? endpoint = ServerEndpoint.tryParse(_server.text);
    setState(() {
      _serverError = endpoint == null
          ? '地址不合法。填服务器根地址即可，例如 lumen.example.com'
          : null;
    });
    if (endpoint != null) {
      ref.read(serverEndpointProvider.notifier).select(endpoint);
    }
  }

  @override
  Widget build(BuildContext context) {
    final ServerEndpoint? endpoint = ref.watch(serverEndpointProvider);
    return Scaffold(
      appBar: AppBar(title: const Text('登录 Lumen')),
      body: Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 420),
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(24),
            child: endpoint == null
                ? _serverForm(context)
                : _accountForm(context, endpoint),
          ),
        ),
      ),
    );
  }

  Widget _serverForm(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: <Widget>[
        Text('服务器地址', style: Theme.of(context).textTheme.titleMedium),
        const SizedBox(height: 8),
        TextField(
          controller: _server,
          autocorrect: false,
          keyboardType: TextInputType.url,
          decoration: InputDecoration(
            hintText: 'lumen.example.com',
            border: const OutlineInputBorder(),
            errorText: _serverError,
            // 不带协议时补的是 https，这一点要说出来——用户以为自己填的是明文地址时
            // 会疑惑为什么连不上自建的 http 服务。
            helperText: '不填协议时按 https 处理',
          ),
          onSubmitted: (_) => _selectServer(),
        ),
        const SizedBox(height: 16),
        FilledButton(onPressed: _selectServer, child: const Text('下一步')),
      ],
    );
  }

  Widget _accountForm(BuildContext context, ServerEndpoint endpoint) {
    final AuthController? auth = ref.watch(authControllerProvider);
    if (auth == null) {
      return const Text('内部错误：服务器已选定但客户端未就绪');
    }
    return StreamBuilder<AuthState>(
      stream: auth.states,
      initialData: auth.state,
      builder: (BuildContext context, AsyncSnapshot<AuthState> snapshot) {
        final AuthState state = snapshot.data ?? const AuthLoggedOut();
        final bool busy = state is AuthBusy;
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: <Widget>[
            _serverLine(context, endpoint),
            if (endpoint.isInsecure) _insecureWarning(context),
            const SizedBox(height: 16),
            TextField(
              controller: _email,
              autocorrect: false,
              enabled: !busy,
              keyboardType: TextInputType.emailAddress,
              decoration: const InputDecoration(
                labelText: '邮箱',
                border: OutlineInputBorder(),
              ),
            ),
            const SizedBox(height: 12),
            TextField(
              controller: _password,
              obscureText: true,
              enabled: !busy,
              decoration: const InputDecoration(
                labelText: '密码',
                border: OutlineInputBorder(),
              ),
              onSubmitted: (_) => _login(auth),
            ),
            const SizedBox(height: 16),
            FilledButton(
              onPressed: busy ? null : () => _login(auth),
              child: busy
                  ? const SizedBox(
                      height: 18,
                      width: 18,
                      child: CircularProgressIndicator(strokeWidth: 2),
                    )
                  : const Text('登录'),
            ),
            if (state is AuthFailed) _failure(context, auth, state),
          ],
        );
      },
    );
  }

  Widget _serverLine(BuildContext context, ServerEndpoint endpoint) {
    return Row(
      children: <Widget>[
        Expanded(
          child: Text(
            endpoint.origin,
            style: Theme.of(context).textTheme.bodyMedium,
            overflow: TextOverflow.ellipsis,
          ),
        ),
        TextButton(
          // 换服务器要清掉当前 endpoint；凭据按 origin 分区，AuthController.restore
          // 只认属于当前 origin 的那份，所以这里不必也不该去删本地 token
          // ——换回来还能用。
          onPressed: () => ref.read(serverEndpointProvider.notifier).clear(),
          child: const Text('换一个'),
        ),
      ],
    );
  }

  Widget _insecureWarning(BuildContext context) {
    // 明文 HTTP 只告警不拦：自建服务器 + 局域网 IP 是真实用法，拦掉会把它变成不可用。
    // 但必须说清代价——token 走 Authorization 头，明文链路上等于裸奔。
    return Padding(
      padding: const EdgeInsets.only(top: 8),
      child: Row(
        children: <Widget>[
          Icon(Icons.warning_amber_rounded,
              size: 18, color: Theme.of(context).colorScheme.error),
          const SizedBox(width: 6),
          Expanded(
            child: Text(
              '这是明文 HTTP 连接，登录凭据在链路上不受保护',
              style: Theme.of(context)
                  .textTheme
                  .bodySmall
                  ?.copyWith(color: Theme.of(context).colorScheme.error),
            ),
          ),
        ],
      ),
    );
  }

  Widget _failure(BuildContext context, AuthController auth, AuthFailed state) {
    final ColorScheme scheme = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.only(top: 16),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: <Widget>[
          Text(
            state.error.userMessage,
            style: TextStyle(color: scheme.error),
          ),
          // 服务端明确说「这个邮箱没注册过」时给出注册入口，而不是让用户自己猜。
          // 注册是**显式**的一步，不静默替他建账号。
          if (state.isUserNotFound) ...<Widget>[
            const SizedBox(height: 8),
            OutlinedButton(
              onPressed: () => auth.registerThenLogin(
                email: _email.text.trim(),
                password: _password.text,
              ),
              child: Text('用 ${_email.text.trim()} 注册新账号'),
            ),
          ],
        ],
      ),
    );
  }

  void _login(AuthController auth) {
    auth.login(email: _email.text.trim(), password: _password.text);
  }
}
