window.__resources = {
  'https://unpkg.com/react@18.3.1/umd/react.production.min.js': '/vendor/react.production.min.js',
  'https://unpkg.com/react-dom@18.3.1/umd/react-dom.production.min.js': '/vendor/react-dom.production.min.js'
};
window.xlaunchCopy = async function(text) {
  if (navigator.clipboard && window.isSecureContext) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const input = document.createElement('textarea');
  input.value = text; input.style.position = 'fixed'; input.style.opacity = '0';
  document.body.appendChild(input); input.select();
  const copied = document.execCommand('copy'); input.remove();
  if (!copied) throw new Error('Clipboard copy was not available.');
};

// Header links shared by every page, derived from /api/config so no page hardcodes a URL.
window.xlaunchSiteLinks = function(config) {
  const handle = (config && config.receiver_handle) || '';
  const github = (config && config.github_url) || '';
  return {
    receiverHandle: handle || '@LaunchOnXMoney',
    hasXProfile: !!handle,
    xProfileUrl: handle ? 'https://x.com/' + handle.replace(/^@/, '') : '',
    hasGithub: !!github,
    githubUrl: github
  };
};

// Backend origin. Empty means same-origin, which is how the Rust server serves
// these pages. The static export injects the tunnel origin ahead of this file.
window.xlaunchApiBase = window.xlaunchApiBase || '';
window.xlaunchApi = function(path) {
  if (typeof path !== 'string' || !path.startsWith('/')) return path;
  return window.xlaunchApiBase + path;
};
