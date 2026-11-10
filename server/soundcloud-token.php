<?php
declare(strict_types=1);

header('Content-Type: application/json; charset=utf-8');
header('Cache-Control: no-store');
header('X-Content-Type-Options: nosniff');

function respond(int $status, array $body): void
{
    http_response_code($status);
    echo json_encode($body);
    exit;
}

$method = $_SERVER['REQUEST_METHOD'];
if ($method !== 'GET' && $method !== 'POST') {
    header('Allow: GET, POST');
    respond(405, ['error' => 'method_not_allowed']);
}

$configPath = dirname(__DIR__, 3) . '/.music-transfer-soundcloud.php';
if (!is_readable($configPath) || !function_exists('curl_init')) {
    respond(503, ['error' => 'oauth_unavailable']);
}
$config = require $configPath;
if ($method === 'GET') {
    respond(200, ['status' => 'ok']);
}

if (strtolower(trim(explode(';', $_SERVER['CONTENT_TYPE'] ?? '', 2)[0])) !== 'application/json') {
    respond(415, ['error' => 'expected_json']);
}
$raw = file_get_contents('php://input', false, null, 0, 4097);
$body = is_string($raw) && strlen($raw) <= 4096 ? json_decode($raw, true) : null;
$grant = is_array($body) ? ($body['grant_type'] ?? '') : '';
$params = [
    'client_id' => $config['client_id'],
    'client_secret' => $config['client_secret'],
    'grant_type' => $grant,
];
if ($grant === 'authorization_code') {
    $verifier = $body['code_verifier'] ?? null;
    $code = $body['code'] ?? null;
    if (!is_string($verifier) || !preg_match('/^[a-zA-Z0-9._~-]{43,128}$/D', $verifier)
        || !is_string($code) || !preg_match('/^[!-~]{1,1024}$/D', $code)) {
        respond(400, ['error' => 'invalid_request']);
    }
    $params['redirect_uri'] = 'http://127.0.0.1:8766/callback';
    $params['code_verifier'] = $verifier;
    $params['code'] = $code;
} elseif ($grant === 'refresh_token') {
    $refresh = $body['refresh_token'] ?? null;
    if (!is_string($refresh) || !preg_match('/^[!-~]{1,2048}$/D', $refresh)) {
        respond(400, ['error' => 'invalid_request']);
    }
    $params['refresh_token'] = $refresh;
} else {
    respond(400, ['error' => 'unsupported_grant_type']);
}

$request = curl_init('https://secure.soundcloud.com/oauth/token');
curl_setopt_array($request, [
    CURLOPT_POST => true,
    CURLOPT_POSTFIELDS => http_build_query($params),
    CURLOPT_HTTPHEADER => ['Accept: application/json', 'Content-Type: application/x-www-form-urlencoded'],
    CURLOPT_RETURNTRANSFER => true,
    CURLOPT_CONNECTTIMEOUT => 5,
    CURLOPT_TIMEOUT => 12,
]);
$raw = curl_exec($request);
$status = (int) curl_getinfo($request, CURLINFO_HTTP_CODE);
curl_close($request);
$token = is_string($raw) ? json_decode($raw, true) : null;
if ($status === 200 && is_array($token) && isset($token['access_token'], $token['refresh_token'], $token['expires_in'])) {
    respond(200, [
        'access_token' => $token['access_token'],
        'refresh_token' => $token['refresh_token'],
        'expires_in' => $token['expires_in'],
    ]);
}
if ($status === 400 || $status === 401) {
    respond(400, ['error' => 'soundcloud_rejected_grant']);
}
respond($status === 429 ? 429 : 502, ['error' => 'soundcloud_unavailable']);
